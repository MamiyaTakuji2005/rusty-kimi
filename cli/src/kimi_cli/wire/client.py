from __future__ import annotations

import asyncio
import contextlib
import json
from typing import Any

from kimi_cli.utils.logging import logger
from kimi_cli.wire import Wire
from kimi_cli.wire.protocol import WIRE_PROTOCOL_VERSION
from kimi_cli.wire.serde import deserialize_wire_message
from kimi_cli.wire.types import (
    ApprovalRequest,
    HookRequest,
    QuestionNotSupported,
    QuestionRequest,
    StatusUpdate,
    ToolCallRequest,
)

# Mirror of WireServer's STDIO_BUFFER_LIMIT: cap how much a single JSON-RPC line
# can buffer when the agent streams a large tool/model payload.
STDIO_BUFFER_LIMIT = 100 * 1024 * 1024


class _WireEncoder(json.JSONEncoder):
    """JSON encoder that knows how to serialize Pydantic models."""

    def default(self, o: Any) -> Any:
        if hasattr(o, "model_dump"):
            return o.model_dump()
        return super().default(o)


class WireClientError(RuntimeError):
    """Raised when the wire connection fails or the agent returns an error."""

    def __init__(self, message: str, *, code: int | None = None) -> None:
        super().__init__(message)
        self.code = code


class WireClient:
    """
    A Wire *client*: the mirror of `kimi_cli.wire.server.WireServer`.

    Spawns an external agent process that speaks the Wire protocol over stdio
    (e.g. ``kimi --wire`` or the Rust ``kimi-agent``) and feeds its ``event`` /
    ``request`` notifications into an in-process `Wire`, so the existing shell UI
    — which only ever reads `WireMessage`s off ``wire.ui_side()`` — renders them
    unchanged. User turns are sent as ``prompt`` requests; approvals and
    questions are relayed back as JSON-RPC responses once the UI resolves them.

    The subprocess is long-lived across turns (it owns the session / context and
    session-file persistence); a single background reader pumps its stdout for
    the lifetime of the connection.
    """

    def __init__(
        self,
        command: list[str],
        *,
        client_name: str = "kimi-shell",
        client_version: str | None = None,
        protocol_version: str = WIRE_PROTOCOL_VERSION,
        cwd: str | None = None,
        env: dict[str, str] | None = None,
    ) -> None:
        self._command = command
        self._client_name = client_name
        self._client_version = client_version
        self._protocol_version = protocol_version
        self._cwd = cwd
        self._env = env

        self._proc: asyncio.subprocess.Process | None = None
        self._reader_task: asyncio.Task[None] | None = None
        self._write_lock = asyncio.Lock()

        self._next_id = 0
        # Futures for responses to requests *we* sent (initialize/prompt/...).
        self._pending: dict[str, asyncio.Future[dict[str, Any]]] = {}
        # Tasks awaiting UI resolution of agent->client requests (approvals etc).
        self._relay_tasks: set[asyncio.Task[None]] = set()

        # The wire for the currently active turn; events route here.
        self._current_wire: Wire | None = None

        # Populated by initialize().
        self.server_info: dict[str, Any] = {}
        self.slash_commands: list[dict[str, Any]] = []
        self.server_capabilities: dict[str, Any] = {}
        self.legacy_mode: bool = False
        """True if the agent does not support ``initialize`` (pre-Wire-1.1)."""
        self.last_status: StatusUpdate | None = None
        """Most recent StatusUpdate seen on the wire (drives the shell status line)."""

    # -- lifecycle ----------------------------------------------------------

    async def connect(self) -> None:
        logger.info("Spawning wire agent: {cmd}", cmd=" ".join(self._command))
        self._proc = await asyncio.create_subprocess_exec(
            *self._command,
            stdin=asyncio.subprocess.PIPE,
            stdout=asyncio.subprocess.PIPE,
            stderr=None,  # inherit: agent diagnostics go straight to our stderr
            cwd=self._cwd,
            env=self._env,
            limit=STDIO_BUFFER_LIMIT,
        )
        self._reader_task = asyncio.create_task(self._read_loop())
        await self._initialize()

    async def close(self) -> None:
        for task in list(self._relay_tasks):
            task.cancel()
        if self._proc is not None:
            with contextlib.suppress(ProcessLookupError):
                if self._proc.stdin is not None and not self._proc.stdin.is_closing():
                    self._proc.stdin.close()
                if self._proc.returncode is None:
                    self._proc.terminate()
            with contextlib.suppress(Exception):
                await asyncio.wait_for(self._proc.wait(), timeout=2.0)
            if self._proc.returncode is None:
                with contextlib.suppress(ProcessLookupError):
                    self._proc.kill()
        if self._reader_task is not None:
            self._reader_task.cancel()
            with contextlib.suppress(asyncio.CancelledError):
                await self._reader_task
        # Fail any still-pending requests.
        for fut in self._pending.values():
            if not fut.done():
                fut.set_exception(WireClientError("Wire connection closed"))
        self._pending.clear()

    # -- outbound requests --------------------------------------------------

    async def _initialize(self) -> None:
        client: dict[str, Any] = {"name": self._client_name}
        if self._client_version is not None:
            client["version"] = self._client_version
        params = {
            "protocol_version": self._protocol_version,
            "client": client,
            "capabilities": {
                "supports_question": True,
                "supports_plan_mode": True,
            },
        }
        try:
            result = await self._request("initialize", params)
        except WireClientError as e:
            if e.code == -32601:  # method not found -> legacy, no-handshake agent
                logger.info("Agent has no initialize method; falling back to legacy mode")
                self.legacy_mode = True
                return
            raise
        self.server_info = result.get("server", {})
        self.slash_commands = result.get("slash_commands", [])
        self.server_capabilities = result.get("capabilities", {})
        logger.info(
            "Connected to wire agent {name} v{ver} (protocol {proto})",
            name=self.server_info.get("name", "?"),
            ver=self.server_info.get("version", "?"),
            proto=result.get("protocol_version", "?"),
        )

    async def prompt(
        self,
        user_input: str | list[Any],
        wire: Wire,
        cancel_event: asyncio.Event,
    ) -> str:
        """
        Run one agent turn. Routes the agent's events into ``wire`` and returns
        the turn status ("finished" | "cancelled" | "max_steps_reached").
        """
        self._current_wire = wire
        try:
            fut = self._new_pending()
            await self._send(
                {
                    "jsonrpc": "2.0",
                    "method": "prompt",
                    "id": fut.msg_id,
                    "params": {"user_input": user_input},
                }
            )
            cancel_task = asyncio.create_task(cancel_event.wait())
            try:
                await asyncio.wait(
                    {fut.future, cancel_task},
                    return_when=asyncio.FIRST_COMPLETED,
                )
                if cancel_event.is_set() and not fut.future.done():
                    with contextlib.suppress(WireClientError):
                        await self._request("cancel")
                result = await fut.future
            finally:
                cancel_task.cancel()
                with contextlib.suppress(asyncio.CancelledError):
                    await cancel_task
            return str(result.get("status", "finished"))
        finally:
            self._current_wire = None

    async def steer(self, user_input: str | list[Any]) -> None:
        await self._request("steer", {"user_input": user_input})

    async def set_plan_mode(self, enabled: bool) -> dict[str, Any]:
        return await self._request("set_plan_mode", {"enabled": enabled})

    async def replay(self, wire: Wire) -> dict[str, Any]:
        self._current_wire = wire
        try:
            return await self._request("replay")
        except WireClientError as e:
            # `replay` is optional (Wire 1.3+). Some agents (e.g. the Rust
            # kimi-agent v1.8.0) implement only initialize/prompt/cancel. Degrade
            # gracefully: no history re-render rather than a crash.
            logger.info("Agent does not support replay ({e}); skipping resume render", e=e)
            return {"status": "unsupported", "events": 0, "requests": 0}
        finally:
            self._current_wire = None

    # -- transport ----------------------------------------------------------

    class _Pending:
        __slots__ = ("msg_id", "future")

        def __init__(self, msg_id: str, future: asyncio.Future[dict[str, Any]]) -> None:
            self.msg_id = msg_id
            self.future = future

    def _new_pending(self) -> WireClient._Pending:
        self._next_id += 1
        msg_id = str(self._next_id)
        fut: asyncio.Future[dict[str, Any]] = asyncio.get_event_loop().create_future()
        self._pending[msg_id] = fut
        return WireClient._Pending(msg_id, fut)

    async def _request(self, method: str, params: Any = None) -> dict[str, Any]:
        pending = self._new_pending()
        msg: dict[str, Any] = {"jsonrpc": "2.0", "method": method, "id": pending.msg_id}
        if params is not None:
            msg["params"] = params
        await self._send(msg)
        return await pending.future

    async def _send(self, obj: dict[str, Any]) -> None:
        if self._proc is None or self._proc.stdin is None:
            raise WireClientError("Wire connection is not open")
        line = (
            json.dumps(obj, separators=(",", ":"), cls=_WireEncoder) + "\n"
        ).encode("utf-8")
        async with self._write_lock:
            self._proc.stdin.write(line)
            await self._proc.stdin.drain()

    async def _respond(self, msg_id: str, *, result: Any = None, error: Any = None) -> None:
        msg: dict[str, Any] = {"jsonrpc": "2.0", "id": msg_id}
        if error is not None:
            msg["error"] = error
        else:
            msg["result"] = result
        await self._send(msg)

    async def _read_loop(self) -> None:
        assert self._proc is not None and self._proc.stdout is not None
        stdout = self._proc.stdout
        try:
            while True:
                line = await stdout.readline()
                if not line:
                    break  # EOF: agent exited
                line = line.strip()
                if not line:
                    continue
                try:
                    obj = json.loads(line)
                except json.JSONDecodeError:
                    logger.warning("Dropping non-JSON line from agent: {line!r}", line=line[:200])
                    continue
                self._handle_incoming(obj)
        except asyncio.CancelledError:
            raise
        except Exception:
            logger.exception("Wire reader loop crashed")
        finally:
            self._fail_pending("Wire agent stream closed")

    def _handle_incoming(self, obj: dict[str, Any]) -> None:
        method = obj.get("method")
        if method == "event":
            self._handle_event(obj.get("params"))
        elif method == "request":
            self._handle_request(obj.get("id"), obj.get("params"))
        elif obj.get("id") is not None and ("result" in obj or "error" in obj):
            self._resolve_response(obj)
        else:
            logger.debug("Ignoring unrecognized message from agent: {obj}", obj=obj)

    def _handle_event(self, params: Any) -> None:
        if params is None:
            return
        try:
            msg = deserialize_wire_message(params)
        except Exception:
            logger.exception("Failed to deserialize event from agent: {params}", params=params)
            return
        if isinstance(msg, StatusUpdate):
            self.last_status = msg
        wire = self._current_wire
        if wire is not None:
            wire.soul_side.send(msg)

    def _handle_request(self, rpc_id: Any, params: Any) -> None:
        if rpc_id is None or params is None:
            return
        try:
            request = deserialize_wire_message(params)
        except Exception:
            logger.exception("Failed to deserialize request from agent: {params}", params=params)
            return
        wire = self._current_wire
        if wire is not None:
            # Push to the UI: it renders the panel and resolves the request object.
            wire.soul_side.send(request)
        task = asyncio.create_task(self._await_and_relay(str(rpc_id), request))
        self._relay_tasks.add(task)
        task.add_done_callback(self._relay_tasks.discard)

    async def _await_and_relay(self, rpc_id: str, request: Any) -> None:
        try:
            match request:
                case ApprovalRequest():
                    kind = await request.wait()
                    await self._respond(
                        rpc_id,
                        result={
                            "request_id": request.id,
                            "response": kind,
                            "feedback": request.feedback,
                        },
                    )
                case QuestionRequest():
                    try:
                        answers = await request.wait()
                    except QuestionNotSupported:
                        answers = {}
                    await self._respond(
                        rpc_id,
                        result={"request_id": request.id, "answers": answers},
                    )
                case HookRequest():
                    action, reason = await request.wait()
                    await self._respond(
                        rpc_id,
                        result={"request_id": request.id, "action": action, "reason": reason},
                    )
                case ToolCallRequest():
                    # External tools require registration via initialize, which the
                    # shell client does not do; reject defensively if one arrives.
                    await self._respond(
                        rpc_id,
                        result={
                            "tool_call_id": request.id,
                            "return_value": {
                                "is_error": True,
                                "output": "External tools are not supported by this client.",
                                "message": "External tools are not supported by this client.",
                                "display": [],
                            },
                        },
                    )
                case _:
                    logger.warning("Unhandled agent request type: {t}", t=type(request).__name__)
        except asyncio.CancelledError:
            raise
        except Exception:
            logger.exception("Failed relaying response for request {id}", id=rpc_id)

    def _resolve_response(self, obj: dict[str, Any]) -> None:
        msg_id = str(obj["id"])
        fut = self._pending.pop(msg_id, None)
        if fut is None:
            logger.debug("Response for unknown id {id}", id=msg_id)
            return
        if fut.done():
            return
        if "error" in obj and obj["error"] is not None:
            err = obj["error"]
            fut.set_exception(
                WireClientError(err.get("message", "agent error"), code=err.get("code"))
            )
        else:
            result = obj.get("result")
            fut.set_result(result if isinstance(result, dict) else {})

    def _fail_pending(self, message: str) -> None:
        for fut in self._pending.values():
            if not fut.done():
                fut.set_exception(WireClientError(message))
        self._pending.clear()
