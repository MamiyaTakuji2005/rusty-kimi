from __future__ import annotations

import asyncio
import contextlib
import json
from typing import Any, Literal, cast

import acp
import pydantic
from kosong.tooling import ToolError, ToolResult
from kosong.utils.typing import JsonType

from kimi_cli.soul import Soul
from kimi_cli.soul.remote import RemoteSoul
from kimi_cli.utils.aioqueue import Queue, QueueShutDown
from kimi_cli.utils.logging import logger
from kimi_cli.utils.signals import install_sigint_handler
from kimi_cli.wire import Wire
from kimi_cli.wire.types import (
    ApprovalRequest,
    ApprovalResponse,
    HookRequest,
    HookResponse,
    QuestionNotSupported,
    QuestionRequest,
    QuestionResponse,
    Request,
    ToolCallRequest,
)

from .jsonrpc import (
    ClientInfo,
    ErrorCodes,
    JSONRPCCancelMessage,
    JSONRPCErrorObject,
    JSONRPCErrorResponse,
    JSONRPCErrorResponseNullableID,
    JSONRPCEventMessage,
    JSONRPCInitializeMessage,
    JSONRPCInMessage,
    JSONRPCInMessageAdapter,
    JSONRPCMessage,
    JSONRPCOutMessage,
    JSONRPCPromptMessage,
    JSONRPCReplayMessage,
    JSONRPCRequestMessage,
    JSONRPCSetPlanModeMessage,
    JSONRPCSteerMessage,
    JSONRPCSuccessResponse,
    Statuses,
)

# Maximum buffer size for the asyncio StreamReader used for stdio.
# Passed as the `limit` argument to `acp.stdio_streams`, this caps how much
# data can be buffered when reading from stdin (e.g., large tool or model
# outputs sent over JSON-RPC). A 100MB limit is large enough for typical
# interactive use while still protecting the process from unbounded memory
# growth or buffer-overrun errors when peers send unexpectedly large payloads.
STDIO_BUFFER_LIMIT = 100 * 1024 * 1024


class WireServer:
    def __init__(self, soul: Soul):
        self._reader: asyncio.StreamReader | None = None
        self._writer: asyncio.StreamWriter | None = None

        # outward
        self._write_task: asyncio.Task[None] | None = None
        self._write_queue: Queue[JSONRPCOutMessage] = Queue()

        # inward
        self._dispatch_tasks: set[asyncio.Task[None]] = set()

        # soul running stuffs
        self._soul = soul
        self._cancel_event: asyncio.Event | None = None
        self._pending_requests: dict[str, Request] = {}
        """Maps JSON RPC message IDs to pending `Request`s."""
        self._client_supports_question: bool = False
        """Whether the Wire client supports QuestionRequest."""
        self._client_supports_plan_mode: bool = False
        """Whether the Wire client supports plan mode."""
        self._initialized: bool = False

    async def serve(self) -> None:
        logger.info("Starting Wire server on stdio")

        self._reader, self._writer = await acp.stdio_streams(limit=STDIO_BUFFER_LIMIT)
        self._write_task = asyncio.create_task(self._write_loop())
        stop_event = asyncio.Event()
        loop = asyncio.get_running_loop()
        remove_sigint = install_sigint_handler(loop, stop_event.set)
        read_task = asyncio.create_task(self._read_loop())
        stop_task = asyncio.create_task(stop_event.wait())
        tasks: set[asyncio.Task[Any]] = {read_task, stop_task}
        pending = tasks
        try:
            done, pending = await asyncio.wait(
                tasks,
                return_when=asyncio.FIRST_COMPLETED,
            )
            if stop_event.is_set():
                logger.info("Wire server interrupted, shutting down")
                if self._cancel_event is not None:
                    self._cancel_event.set()
                if not read_task.done():
                    read_task.cancel()
                    with contextlib.suppress(asyncio.CancelledError):
                        await read_task
            elif read_task in done:
                read_task.result()
        except KeyboardInterrupt:
            logger.info("Wire server interrupted, shutting down")
            if self._cancel_event is not None:
                self._cancel_event.set()
        finally:
            remove_sigint()
            for task in pending:
                task.cancel()
                with contextlib.suppress(asyncio.CancelledError):
                    await task
            await self._shutdown()

    async def _write_loop(self) -> None:
        assert self._writer is not None

        try:
            while True:
                try:
                    msg = await self._write_queue.get()
                except QueueShutDown:
                    logger.debug("Send queue shut down, stopping Wire server write loop")
                    break
                self._writer.write(msg.model_dump_json().encode("utf-8") + b"\n")
                await self._writer.drain()
        except asyncio.CancelledError:
            raise
        except Exception:
            logger.exception("Wire server write loop error:")
            raise

    async def _read_loop(self) -> None:
        assert self._reader is not None

        while True:
            raw_line = await self._reader.readline()
            if not raw_line:
                logger.info("stdin closed, Wire server exiting")
                break
            line = raw_line.decode("utf-8", errors="replace").strip()

            try:
                msg_json = json.loads(line)
            except ValueError:
                logger.error("Invalid JSON line: {line}", line=line)
                await self._send_msg(
                    JSONRPCErrorResponseNullableID(
                        id=None,
                        error=JSONRPCErrorObject(
                            code=ErrorCodes.PARSE_ERROR,
                            message="Invalid JSON format",
                        ),
                    )
                )
                continue

            try:
                generic_msg = JSONRPCMessage.model_validate(msg_json)
            except pydantic.ValidationError as e:
                logger.error("Invalid JSON-RPC message: {error}", error=e)
                await self._send_msg(
                    JSONRPCErrorResponseNullableID(
                        id=None,
                        error=JSONRPCErrorObject(
                            code=ErrorCodes.INVALID_REQUEST,
                            message="Invalid request",
                        ),
                    )
                )
                continue

            if generic_msg.is_response():
                # for responses, we skip the method check
                try:
                    msg = JSONRPCInMessageAdapter.validate_python(msg_json)
                except pydantic.ValidationError as e:
                    logger.error("Invalid JSON-RPC response: {error}", error=e)
                    await self._send_msg(
                        JSONRPCErrorResponseNullableID(
                            id=None,
                            error=JSONRPCErrorObject(
                                code=ErrorCodes.INVALID_REQUEST,
                                message="Invalid response",
                            ),
                        )
                    )
                    continue  # ignore invalid json-rpc responses

                if not isinstance(msg, (JSONRPCSuccessResponse, JSONRPCErrorResponse)):
                    logger.error(
                        "Invalid JSON-RPC response message: {msg}",
                        msg=msg_json,
                    )
                    continue  # ignore invalid response messages

                task = asyncio.create_task(self._dispatch_msg(msg))
                task.add_done_callback(self._dispatch_tasks.discard)
                self._dispatch_tasks.add(task)
                continue

            if not generic_msg.method_is_inbound():
                logger.error(
                    "Unexpected JSON-RPC method received: {method}",
                    method=generic_msg.method,
                )
                if generic_msg.id is not None:
                    resp = JSONRPCErrorResponse(
                        id=generic_msg.id,
                        error=JSONRPCErrorObject(
                            code=ErrorCodes.METHOD_NOT_FOUND,
                            message=f"Unexpected method received: {generic_msg.method}",
                        ),
                    )
                    await self._send_msg(resp)
                continue  # ignore unexpected outbound methods

            try:
                msg = JSONRPCInMessageAdapter.validate_python(msg_json)
            except pydantic.ValidationError as e:
                logger.error("Invalid JSON-RPC inbound message: {error}", error=e)
                if generic_msg.id is not None:
                    resp = JSONRPCErrorResponse(
                        id=generic_msg.id,
                        error=JSONRPCErrorObject(
                            code=ErrorCodes.INVALID_PARAMS,
                            message=f"Invalid parameters for method `{generic_msg.method}`",
                        ),
                    )
                    await self._send_msg(resp)
                continue  # ignore invalid inbound messages

            task = asyncio.create_task(self._dispatch_msg(msg))
            task.add_done_callback(self._dispatch_tasks.discard)
            self._dispatch_tasks.add(task)

    async def _shutdown(self) -> None:
        for request in self._pending_requests.values():
            if request.resolved:
                continue
            match request:
                case ApprovalRequest():
                    if request.source_kind == "foreground_turn":
                        request.resolve("reject")
                case ToolCallRequest():
                    request.resolve(
                        ToolError(
                            message="Wire connection closed before tool result was received.",
                            brief="Wire closed",
                        )
                    )
                case QuestionRequest():
                    request.resolve({})
                case HookRequest():
                    request.resolve("allow")
        self._pending_requests.clear()

        if self._cancel_event is not None:
            self._cancel_event.set()
            self._cancel_event = None

        self._write_queue.shutdown()
        if self._write_task is not None:
            with contextlib.suppress(asyncio.CancelledError):
                await self._write_task

        await asyncio.gather(*self._dispatch_tasks, return_exceptions=True)
        self._dispatch_tasks.clear()

        if self._writer is not None:
            self._writer.close()
            with contextlib.suppress(Exception):
                await self._writer.wait_closed()
            self._writer = None

        self._reader = None
        self._initialized = False

    async def _dispatch_msg(self, msg: JSONRPCInMessage) -> None:
        resp: JSONRPCSuccessResponse | JSONRPCErrorResponse | None = None
        try:
            match msg:
                case JSONRPCInitializeMessage():
                    resp = await self._handle_initialize(msg)
                case JSONRPCPromptMessage():
                    resp = await self._handle_prompt(msg)
                case JSONRPCReplayMessage():
                    resp = await self._handle_replay(msg)
                case JSONRPCSteerMessage():
                    resp = await self._handle_steer(msg)
                case JSONRPCSetPlanModeMessage():
                    resp = await self._handle_set_plan_mode(msg)
                case JSONRPCCancelMessage():
                    resp = await self._handle_cancel(msg)
                case JSONRPCSuccessResponse() | JSONRPCErrorResponse():
                    await self._handle_response(msg)

            if resp is not None:
                await self._send_msg(resp)
        except Exception:
            logger.exception("Unexpected error dispatching JSONRPC message:")
            raise

    async def _send_msg(self, msg: JSONRPCOutMessage) -> None:
        try:
            await self._write_queue.put(msg)
        except QueueShutDown:
            logger.error("Send queue shut down; dropping message: {msg}", msg=msg)

    @property
    def _is_streaming(self) -> bool:
        return self._cancel_event is not None

    async def _handle_initialize(
        self, msg: JSONRPCInitializeMessage
    ) -> JSONRPCSuccessResponse | JSONRPCErrorResponse:
        if self._is_streaming:
            return JSONRPCErrorResponse(
                id=msg.id,
                error=JSONRPCErrorObject(
                    code=ErrorCodes.INVALID_STATE,
                    message="An agent turn is already in progress",
                ),
            )

        slash_commands: list[JsonType] = []
        for cmd in self._soul.available_slash_commands:
            slash_commands.append(
                cast(
                    JsonType,
                    {"name": cmd.name, "description": cmd.description, "aliases": cmd.aliases},
                )
            )

        from kimi_cli.constant import NAME, VERSION
        from kimi_cli.hooks.config import HOOK_EVENT_TYPES
        from kimi_cli.hooks.engine import WireHookHandle, WireHookSubscription
        from kimi_cli.soul import wire_send
        from kimi_cli.wire.protocol import WIRE_PROTOCOL_VERSION
        from kimi_cli.wire.types import HookResolved, HookTriggered

        # Hook engine setup — register wire subscriptions and callbacks

        hook_engine = self._soul.hook_engine

        if msg.params.hooks:
            wire_subs: list[WireHookSubscription] = []
            for wh in msg.params.hooks:
                if wh.event not in HOOK_EVENT_TYPES:
                    logger.warning("Ignoring unknown hook event from client: {}", wh.event)
                    continue
                wire_subs.append(
                    WireHookSubscription(
                        id=wh.id,
                        event=wh.event,
                        matcher=wh.matcher,
                        timeout=wh.timeout,
                    )
                )
            if wire_subs:
                hook_engine.add_wire_subscriptions(wire_subs)
                logger.info("Registered {} wire hook subscriptions from client", len(wire_subs))

        def _on_triggered(event: str, target: str, count: int) -> None:
            wire_send(HookTriggered(event=event, target=target, hook_count=count))

        def _on_resolved(
            event: str,
            target: str,
            action: str,
            reason: str,
            duration_ms: int,
        ) -> None:
            wire_send(
                HookResolved(
                    event=event,
                    target=target,
                    action=cast(Literal["allow", "block"], action),
                    reason=reason,
                    duration_ms=duration_ms,
                )
            )

        async def _on_wire_hook(handle: WireHookHandle) -> None:
            """Send HookRequest to client, wire response back to handle."""
            request = HookRequest(
                id=handle.id,
                subscription_id=handle.subscription_id,
                event=handle.event,
                target=handle.target,
                input_data=handle.input_data,
            )
            self._pending_requests[handle.id] = request
            await self._send_msg(JSONRPCRequestMessage(id=handle.id, params=request))
            # Wait for client response (resolved via _handle_response)
            action, reason = await request.wait()
            handle.resolve(action, reason)

        hook_engine.set_callbacks(
            on_triggered=_on_triggered,
            on_resolved=_on_resolved,
            on_wire_hook=_on_wire_hook,
        )

        hooks_info: dict[str, JsonType] = cast(
            dict[str, JsonType],
            {
                "supported_events": HOOK_EVENT_TYPES,
                "configured": hook_engine.summary,
            },
        )

        result: dict[str, JsonType] = {
            "protocol_version": WIRE_PROTOCOL_VERSION,
            "server": cast(JsonType, {"name": NAME, "version": VERSION}),
            "slash_commands": cast(JsonType, slash_commands),
        }

        if hooks_info:
            result["hooks"] = cast(JsonType, hooks_info)

        self._apply_wire_client_info(msg.params.client)
        self._track_session_started(msg.params.client)

        if msg.params.capabilities is not None:
            self._client_supports_question = msg.params.capabilities.supports_question
            self._client_supports_plan_mode = msg.params.capabilities.supports_plan_mode

        self._initialized = True

        result["capabilities"] = cast(
            JsonType,
            {"supports_question": True},
        )

        return JSONRPCSuccessResponse(
            id=msg.id,
            result=result,
        )

    def _apply_wire_client_info(self, client: ClientInfo | None) -> None:
        if client is not None:
            from kimi_cli.telemetry import set_client_info

            set_client_info(name=client.name, version=client.version)

    def _track_session_started(self, client: ClientInfo | None) -> None:
        from kimi_cli.telemetry import track_session_started_once

        track_session_started_once(
            ui_mode="wire",
            resumed=False,
            client_name=client.name if client is not None else None,
            client_version=client.version if client is not None else None,
        )

    async def _handle_prompt(
        self, msg: JSONRPCPromptMessage
    ) -> JSONRPCSuccessResponse | JSONRPCErrorResponse:
        if self._is_streaming:
            # TODO: support queueing multiple inputs
            return JSONRPCErrorResponse(
                id=msg.id,
                error=JSONRPCErrorObject(
                    code=ErrorCodes.INVALID_STATE, message="An agent turn is already in progress"
                ),
            )

        try:
            return JSONRPCErrorResponse(
                id=msg.id,
                error=JSONRPCErrorObject(
                    code=ErrorCodes.INVALID_STATE,
                    message=(
                        "Local LLM execution is disabled; run via the Rust agent (KIMI_AGENT_BIN)."
                    ),
                ),
            )
        finally:
            # Clean up any remaining pending requests from this turn.
            stale_ids = [k for k, v in self._pending_requests.items() if not v.resolved]
            for msg_id in stale_ids:
                request = self._pending_requests[msg_id]
                match request:
                    case ApprovalRequest():
                        if request.source_kind == "foreground_turn":
                            self._pending_requests.pop(msg_id, None)
                            request.resolve("reject")
                    case ToolCallRequest():
                        self._pending_requests.pop(msg_id, None)
                        request.resolve(
                            ToolError(
                                message="Agent turn ended before tool result was received.",
                                brief="Turn ended",
                            )
                        )
                    case QuestionRequest():
                        self._pending_requests.pop(msg_id, None)
                        request.resolve({})
                    case HookRequest():
                        self._pending_requests.pop(msg_id, None)
                        request.resolve("allow")
                    case _:
                        pass
            self._cancel_event = None

    async def _handle_steer(
        self, msg: JSONRPCSteerMessage
    ) -> JSONRPCSuccessResponse | JSONRPCErrorResponse:
        if not self._is_streaming:
            return JSONRPCErrorResponse(
                id=msg.id,
                error=JSONRPCErrorObject(
                    code=ErrorCodes.INVALID_STATE,
                    message="No agent turn is in progress",
                ),
            )

        soul = cast(RemoteSoul, self._soul)
        soul.steer(msg.params.user_input)
        return JSONRPCSuccessResponse(
            id=msg.id,
            result={"status": Statuses.STEERED},
        )

    async def _handle_set_plan_mode(
        self, msg: JSONRPCSetPlanModeMessage
    ) -> JSONRPCSuccessResponse | JSONRPCErrorResponse:
        return JSONRPCErrorResponse(
            id=msg.id,
            error=JSONRPCErrorObject(
                code=ErrorCodes.INVALID_STATE,
                message="Plan mode is not supported",
            ),
        )

    async def _handle_replay(
        self, msg: JSONRPCReplayMessage
    ) -> JSONRPCSuccessResponse | JSONRPCErrorResponse:
        if self._is_streaming:
            return JSONRPCErrorResponse(
                id=msg.id,
                error=JSONRPCErrorObject(
                    code=ErrorCodes.INVALID_STATE, message="An agent turn is already in progress"
                ),
            )

        return JSONRPCSuccessResponse(
            id=msg.id,
            result={"status": Statuses.FINISHED, "events": 0, "requests": 0},
        )

    async def _handle_cancel(
        self, msg: JSONRPCCancelMessage
    ) -> JSONRPCSuccessResponse | JSONRPCErrorResponse:
        if not self._is_streaming:
            return JSONRPCErrorResponse(
                id=msg.id,
                error=JSONRPCErrorObject(
                    code=ErrorCodes.INVALID_STATE, message="No agent turn is in progress"
                ),
            )

        assert self._cancel_event is not None
        self._cancel_event.set()
        return JSONRPCSuccessResponse(
            id=msg.id,
            result={},
        )

    async def _handle_response(self, msg: JSONRPCSuccessResponse | JSONRPCErrorResponse) -> None:
        request = self._pending_requests.pop(msg.id, None)
        if request is None:
            logger.error("No pending request for response id={id}", id=msg.id)
            return

        match request:
            case ApprovalRequest():
                if isinstance(msg, JSONRPCErrorResponse):
                    request.resolve("reject")
                    return

                try:
                    result = ApprovalResponse.model_validate(msg.result)
                except pydantic.ValidationError as e:
                    logger.error(
                        "Invalid response result for request id={id}: {error}",
                        id=msg.id,
                        error=e,
                    )
                    request.resolve("reject")
                    return

                if result.request_id != request.id:
                    logger.warning(
                        "Approval response id mismatch: request={request_id}, "
                        "response={response_id}",
                        request_id=request.id,
                        response_id=result.request_id,
                    )
                request.resolve(result.response)
            case ToolCallRequest():
                if isinstance(msg, JSONRPCErrorResponse):
                    error = msg.error.message
                    request.resolve(
                        ToolError(
                            message=error,
                            brief="External tool error",
                        )
                    )
                    return

                try:
                    tool_result = ToolResult.model_validate(msg.result)
                except pydantic.ValidationError as e:
                    logger.error(
                        "Invalid tool result for request id={id}: {error}",
                        id=msg.id,
                        error=e,
                    )
                    request.resolve(
                        ToolError(
                            message="Invalid tool result payload from client.",
                            brief="Invalid tool result",
                        )
                    )
                    return
                if tool_result.tool_call_id != request.id:
                    logger.warning(
                        "Tool result id mismatch: request={request_id}, result={result_id}",
                        request_id=request.id,
                        result_id=tool_result.tool_call_id,
                    )
                request.resolve(tool_result.return_value)
            case QuestionRequest():
                if isinstance(msg, JSONRPCErrorResponse):
                    request.resolve({})
                    return

                try:
                    result = QuestionResponse.model_validate(msg.result)
                except pydantic.ValidationError as e:
                    logger.error(
                        "Invalid question response for request id={id}: {error}",
                        id=msg.id,
                        error=e,
                    )
                    request.resolve({})
                    return

                if result.request_id != request.id:
                    logger.warning(
                        "Question response id mismatch: request={request_id}, "
                        "response={response_id}",
                        request_id=request.id,
                        response_id=result.request_id,
                    )
                request.resolve(result.answers)
            case HookRequest():
                if isinstance(msg, JSONRPCErrorResponse):
                    request.resolve("allow")
                    return

                try:
                    result = HookResponse.model_validate(msg.result)
                except pydantic.ValidationError as e:
                    logger.error(
                        "Invalid hook response for request id={id}: {error}",
                        id=msg.id,
                        error=e,
                    )
                    request.resolve("allow")
                    return

                if result.request_id != request.id:
                    logger.warning(
                        "Hook response id mismatch: request={request_id}, response={response_id}",
                        request_id=request.id,
                        response_id=result.request_id,
                    )
                request.resolve(result.action, result.reason)

    async def _stream_wire_messages(self, wire: Wire) -> None:
        wire_ui = wire.ui_side(merge=False)
        while True:
            msg = await wire_ui.receive()
            match msg:
                case ApprovalRequest():
                    await self._request_approval(msg)
                case ToolCallRequest():
                    await self._request_external_tool(msg)
                case QuestionRequest():
                    await self._request_question(msg)
                case HookRequest():
                    pass  # handled via hook engine callbacks
                case _:
                    await self._send_msg(JSONRPCEventMessage(method="event", params=msg))

    async def _request_approval(self, request: ApprovalRequest) -> None:
        msg_id = request.id  # just use the approval request id as message id
        self._pending_requests[msg_id] = request
        await self._send_msg(JSONRPCRequestMessage(id=msg_id, params=request))
        # Do NOT await request.wait() here.  The approval future is awaited by
        # the tool that created the request (inside the soul task).  Blocking the
        # UI loop would prevent ALL subsequent Wire messages — from every
        # concurrent subagent — from reaching stdout, causing a cascade deadlock
        # when the approval response is lost (e.g. no WebSocket connected).

    async def _request_external_tool(self, request: ToolCallRequest) -> None:
        msg_id = request.id
        self._pending_requests[msg_id] = request
        await self._send_msg(JSONRPCRequestMessage(id=msg_id, params=request))
        # Same rationale as _request_approval: do not block the UI loop.

    async def _request_question(self, request: QuestionRequest) -> None:
        if not self._client_supports_question:
            # Client does not support interactive questions; signal the tool
            # so it can tell the LLM to use an alternative approach.
            request.set_exception(QuestionNotSupported())
            return
        msg_id = request.id
        self._pending_requests[msg_id] = request
        await self._send_msg(JSONRPCRequestMessage(id=msg_id, params=request))
        # Same rationale as _request_approval: do not block the UI loop.
