from __future__ import annotations

import asyncio
import contextlib
import sys
import warnings
from collections.abc import AsyncGenerator, Callable
from pathlib import Path
from typing import TYPE_CHECKING, Any

import kaos
from kaos.path import KaosPath

from kimi_cli.auth.oauth import OAuthManager
from kimi_cli.background.models import is_terminal_status
from kimi_cli.cli import InputFormat, OutputFormat
from kimi_cli.config import Config, load_config
from kimi_cli.session import Session
from kimi_cli.share import get_share_dir
from kimi_cli.soul import RunCancelled, Soul, run_soul
from kimi_cli.soul.agent import Runtime
from kimi_cli.utils.aioqueue import QueueShutDown
from kimi_cli.utils.logging import logger, open_original_stderr, redirect_stderr_to_logger
from kimi_cli.utils.path import shorten_home
from kimi_cli.utils.signals import install_sigint_handler
from kimi_cli.wire import Wire, WireUISide
from kimi_cli.wire.types import ApprovalRequest, ApprovalResponse, ContentPart, WireMessage

if TYPE_CHECKING:
    from fastmcp.mcp_config import MCPConfig


def _patch_session_id(record: dict[str, Any]) -> None:
    """Inject the current session ID (from ContextVar) into log records."""
    try:
        from kimi_cli.soul.toolset import get_session_id

        sid = get_session_id()
        record["extra"]["sid"] = sid if sid else ""
    except Exception:
        record["extra"].setdefault("sid", "")


def enable_logging(debug: bool = False, *, redirect_stderr: bool = True) -> None:
    # NOTE: stderr redirection is implemented by swapping the process-level fd=2 (dup2).
    # That can hide Click/Typer error output during CLI startup, so some entrypoints delay
    # installing it until after critical initialization succeeds.
    logger.remove()  # Remove default stderr handler
    logger.enable("kimi_cli")
    if debug:
        logger.enable("kosong")
    logger.add(
        get_share_dir() / "logs" / "kimi.log",
        # FIXME: configure level for different modules
        level="TRACE" if debug else "INFO",
        format=(
            "{time:YYYY-MM-DD HH:mm:ss.SSS} | {level: <8} | "
            "{name}:{function}:{line} | {extra[sid]} - {message}"
        ),
        rotation="06:00",
        retention="10 days",
    )
    logger.configure(extra={"sid": ""}, patcher=_patch_session_id)
    if redirect_stderr:
        redirect_stderr_to_logger()


def _write_original_stderr(text: str) -> None:
    """Write a user-facing notice to the terminal even if ``fd=2`` has been
    redirected into the logger by ``redirect_stderr_to_logger``.

    Falls back to ``sys.stderr`` when no redirector is installed (tests,
    early-startup code paths), matching the semantics of ``_emit_fatal_error``
    in ``cli/__init__.py``.
    """
    with open_original_stderr() as stream:
        if stream is not None:
            stream.write(text.encode("utf-8", errors="replace"))
            stream.flush()
            return
    sys.stderr.write(text)


class KimiCLI:
    @staticmethod
    async def create(
        session: Session,
        *,
        # Basic configuration
        config: Config | Path | None = None,
        model_name: str | None = None,
        thinking: bool | None = None,
        # Run mode
        yolo: bool = False,
        afk: bool = False,
        runtime_afk: bool = False,
        plan_mode: bool = False,
        resumed: bool = False,
        ui_mode: str = "shell",
        # Extensions
        agent_file: Path | None = None,
        mcp_configs: list[MCPConfig] | list[dict[str, Any]] | None = None,
        skills_dirs: list[KaosPath] | None = None,
        # Loop control
        max_steps_per_turn: int | None = None,
        max_retries_per_step: int | None = None,
        max_ralph_iterations: int | None = None,
        startup_progress: Callable[[str], None] | None = None,
        defer_mcp_loading: bool = False,
        remote_agent: str,
    ) -> KimiCLI:
        """
        Create a KimiCLI instance.

        Args:
            session (Session): A session created by `Session.create` or `Session.continue_`.
            config (Config | Path | None, optional): Configuration to use, or path to config file.
                Defaults to None.
            model_name (str | None, optional): Name of the model to use. Defaults to None.
            thinking (bool | None, optional): Whether to enable thinking mode. Defaults to None.
            yolo (bool, optional): Approve all actions without confirmation. Defaults to False.
            afk (bool, optional): Invocation-level away-from-keyboard mode (no user is present
                to answer questions or approve actions). Implies auto-approve. Defaults to False.
            runtime_afk (bool, optional): Internal invocation-only afk overlay, used by print mode
                so it stays non-interactive without changing persisted session afk. Defaults to
                False.
            agent_file (Path | None, optional): Path to the agent file. Defaults to None.
            mcp_configs (list[MCPConfig | dict[str, Any]] | None, optional): MCP configs to load
                MCP tools from. Defaults to None.
            skills_dirs (list[KaosPath] | None, optional): Custom skills directories that
                override default user/project discovery. Defaults to None.
            max_steps_per_turn (int | None, optional): Maximum number of steps in one turn.
                Defaults to None.
            max_retries_per_step (int | None, optional): Maximum number of retries in one step.
                Defaults to None.
            max_ralph_iterations (int | None, optional): Extra iterations after the first turn in
                Ralph mode. Defaults to None.
            startup_progress (Callable[[str], None] | None, optional): Progress callback used by
                interactive startup UI. Defaults to None.
            defer_mcp_loading (bool, optional): Defer MCP startup until the interactive shell is
                ready. Defaults to False.

        Raises:
            FileNotFoundError: When the agent file is not found.
            ConfigError(KimiCLIException, ValueError): When the configuration is invalid.
            AgentSpecError(KimiCLIException, ValueError): When the agent specification is invalid.
            SystemPromptTemplateError(KimiCLIException, ValueError): When the system prompt
                template is invalid.
            InvalidToolError(KimiCLIException, ValueError): When any tool cannot be loaded.
            MCPConfigError(KimiCLIException, ValueError): When any MCP configuration is invalid.
            MCPRuntimeError(KimiCLIException, RuntimeError): When any MCP server cannot be
                connected.
        """
        if startup_progress is not None:
            startup_progress("Loading configuration...")

        config = config if isinstance(config, Config) else load_config(config)
        if max_steps_per_turn is not None:
            config.loop_control.max_steps_per_turn = max_steps_per_turn
        if max_retries_per_step is not None:
            config.loop_control.max_retries_per_step = max_retries_per_step
        if max_ralph_iterations is not None:
            config.loop_control.max_ralph_iterations = max_ralph_iterations
        logger.info("Loaded config: {config}", config=config)

        oauth = OAuthManager(config)

        # Remote mode: the external Wire agent owns the loop, LLM, tools, MCP
        # and session. Skip all local LLM/agent/soul construction.
        return await KimiCLI._create_remote(
            remote_agent,
            config=config,
            oauth=oauth,
            session=session,
            yolo=yolo,
            afk=afk,
            runtime_afk=runtime_afk,
            skills_dirs=skills_dirs,
            ui_mode=ui_mode,
            resumed=resumed,
            startup_progress=startup_progress,
        )

    @staticmethod
    async def _create_remote(
        remote_agent: str,
        *,
        config: Config,
        oauth: OAuthManager,
        session: Session,
        yolo: bool,
        afk: bool,
        runtime_afk: bool,
        skills_dirs: list[KaosPath] | None,
        ui_mode: str,
        resumed: bool,
        startup_progress: Callable[[str], None] | None,
    ) -> KimiCLI:
        """
        Build a `KimiCLI` that drives an external Wire agent (`KIMI_AGENT_BIN`).

        No local LLM, agent, tools, MCP or `KimiSoul` are created — the agent
        process owns all of that. We keep only a light `Runtime` (for the local
        session / work dir / approval state the shell furniture reads) and a
        `RemoteSoul` shim over a connected `WireClient`.
        """
        import shlex

        from kimi_cli.soul.remote import RemoteSoul
        from kimi_cli.wire.client import WireClient

        runtime = await Runtime.create(
            config,
            oauth,
            None,  # no local LLM
            session,
            yolo,
            afk=afk,
            runtime_afk=runtime_afk,
            skills_dirs=skills_dirs,
        )
        runtime.ui_mode = ui_mode
        runtime.resumed = resumed
        runtime.notifications.recover()
        runtime.background_tasks.reconcile()

        if startup_progress is not None:
            startup_progress("Connecting to agent...")
        tokens = [t.strip('"') for t in shlex.split(remote_agent, posix=False)]
        # Ensure the Rust agent uses the same session as Python
        tokens.extend(["--session", session.id])
        client = WireClient(tokens, client_name="kimi-shell", cwd=str(session.work_dir))
        await client.connect()

        instance = KimiCLI(RemoteSoul(client), runtime, {}, None)
        instance._remote_client = client
        return instance

    def __init__(
        self,
        _soul: Soul,
        _runtime: Runtime,
        _env_overrides: dict[str, str],
        _bg_refresh_task: asyncio.Task[None] | None = None,
    ) -> None:
        self._soul = _soul
        self._runtime = _runtime
        self._env_overrides = _env_overrides
        self._bg_refresh_task = _bg_refresh_task
        self._remote_client: Any | None = None
        """Set when running against an external Wire agent (KIMI_AGENT_BIN)."""

    @property
    def soul(self) -> Soul:
        """Get the Soul instance."""
        return self._soul

    @property
    def session(self) -> Session:
        """Get the Session instance."""
        return self._runtime.session

    async def shutdown_background_tasks(self) -> None:
        """Kill active background tasks on exit, unless keep_alive_on_exit is configured.

        Prints a stderr notice naming each task so the user knows what is being
        terminated, waits out the configured kill grace period so SIGTERM can
        take effect, then reconciles and reports any workers that ignored the
        signal.

        This runs on the CLI's hard-shutdown path, so every failure mode must
        be contained: disk IO errors from ``list_tasks`` / ``reconcile`` or
        store corruption must not propagate and replace the real exit code
        with a traceback.
        """
        # Cancel the startup managed-model refresh task if it is still running
        # so it does not outlive the CLI process.
        if self._bg_refresh_task is not None and not self._bg_refresh_task.done():
            self._bg_refresh_task.cancel()

        bg_config = self._runtime.config.background
        if bg_config.keep_alive_on_exit:
            return

        try:
            manager = self._runtime.background_tasks
            active_views = [
                v
                for v in manager.list_tasks(status=None, limit=None)
                if not is_terminal_status(v.runtime.status)
            ]
            if not active_views:
                return

            # Split by whether the task has already been kill-requested (e.g.
            # by the ``--print`` timeout path which ran immediately before
            # this shutdown).  For those:
            #   - don't re-announce on stderr (user saw the timeout notice)
            #   - don't re-kill with a generic reason, which would overwrite
            #     the more specific ``kill_reason`` on disk
            # We still reconcile + grace-wait for them so they reach terminal
            # status before the process exits.
            fresh_targets = [v for v in active_views if v.control.kill_requested_at is None]

            if fresh_targets:
                # Build and emit the kill notice via ``open_original_stderr``
                # — ``sys.stderr.write`` alone would silently land in
                # ``kimi.log`` because ``redirect_stderr_to_logger`` has
                # replaced fd=2 with a pipe into the logger by this point.
                lines = [f"\u26a0  Killing {len(fresh_targets)} background tasks:\n"]
                for view in fresh_targets:
                    description = view.spec.description or ""
                    if len(description) > 60:
                        description = description[:57] + "..."
                    lines.append(f"  {view.spec.id}  {description}\n")
                _write_original_stderr("".join(lines))

                killed: list[str] = []
                for view in fresh_targets:
                    try:
                        manager.kill(view.spec.id, reason="CLI session ended")
                        killed.append(view.spec.id)
                    except Exception:
                        logger.exception(
                            "Failed to kill task {task_id} during shutdown",
                            task_id=view.spec.id,
                        )
                if killed:
                    logger.info(
                        "Stopped {n} background task(s) on exit: {ids}",
                        n=len(killed),
                        ids=killed,
                    )

            await asyncio.sleep(bg_config.kill_grace_period_ms / 1000)
            manager.reconcile()
            survivors = [
                v
                for v in manager.list_tasks(status=None, limit=None)
                if not is_terminal_status(v.runtime.status)
            ]
            if survivors:
                # Distinguish "worker is mid-shutdown" (kill request on record,
                # SIGTERM delivered, worker just hasn't written terminal state
                # yet) from a genuine leak (never got kill-requested, i.e.
                # ``manager.kill`` raised).  Without this split, users saw
                # ``killed N`` from the --print timeout path immediately
                # followed by ``(N tasks still alive)`` here — a direct
                # semantic contradiction.
                terminating = [s for s in survivors if s.control.kill_requested_at is not None]
                leaking = [s for s in survivors if s.control.kill_requested_at is None]
                # Report leaks first — ``stop request failed`` is strictly
                # more severe than ``still terminating`` (the latter will
                # resolve on its own once the worker writes terminal state).
                if leaking:
                    _write_original_stderr(
                        f"  ({len(leaking)} tasks still running; stop request failed)\n"
                    )
                if terminating:
                    _write_original_stderr(f"  ({len(terminating)} tasks still terminating)\n")
        except Exception:
            logger.warning("Error during background task shutdown; continuing exit", exc_info=True)

    async def await_bg_tasks_shutdown(self, timeout: float = 2.0) -> None:
        """Await completion of the model-refresh background task after cancellation."""
        task = self._bg_refresh_task
        if task is None or task.done():
            return
        # Best-effort cleanup — errors inside the task are already logged.
        with contextlib.suppress(TimeoutError, asyncio.CancelledError, Exception):
            await asyncio.wait_for(asyncio.shield(task), timeout=timeout)

    @contextlib.asynccontextmanager
    async def _env(self) -> AsyncGenerator[None]:
        original_cwd = KaosPath.cwd()
        await kaos.chdir(self._runtime.session.work_dir)
        try:
            # to ignore possible warnings from dateparser
            warnings.filterwarnings("ignore", category=DeprecationWarning)
            async with self._runtime.oauth.refreshing(self._runtime):
                yield
        finally:
            await kaos.chdir(original_cwd)

    async def run(
        self,
        user_input: str | list[ContentPart],
        cancel_event: asyncio.Event,
        merge_wire_messages: bool = False,
    ) -> AsyncGenerator[WireMessage]:
        """
        Run the Kimi Code CLI instance without any UI and yield Wire messages directly.

        Args:
            user_input (str | list[ContentPart]): The user input to the agent.
            cancel_event (asyncio.Event): An event to cancel the run.
            merge_wire_messages (bool): Whether to merge Wire messages as much as possible.

        Yields:
            WireMessage: The Wire messages from the `KimiSoul`.

        Raises:
            LLMNotSet: When the LLM is not set.
            LLMNotSupported: When the LLM does not have required capabilities.
            ChatProviderError: When the LLM provider returns an error.
            MaxStepsReached: When the maximum number of steps is reached.
            RunCancelled: When the run is cancelled by the cancel event.
        """
        async with self._env():
            wire_future = asyncio.Future[WireUISide]()
            stop_ui_loop = asyncio.Event()
            approval_bridge_tasks: dict[str, asyncio.Task[None]] = {}
            forwarded_approval_requests: dict[str, ApprovalRequest] = {}

            async def _bridge_approval_request(request: ApprovalRequest) -> None:
                try:
                    response = await request.wait()
                    assert self._runtime.approval_runtime is not None
                    self._runtime.approval_runtime.resolve(
                        request.id, response, feedback=request.feedback
                    )
                finally:
                    approval_bridge_tasks.pop(request.id, None)
                    forwarded_approval_requests.pop(request.id, None)

            def _forward_approval_request(wire: Wire, request: ApprovalRequest) -> None:
                if request.id in forwarded_approval_requests:
                    return
                forwarded_approval_requests[request.id] = request
                if request.id not in approval_bridge_tasks:
                    approval_bridge_tasks[request.id] = asyncio.create_task(
                        _bridge_approval_request(request)
                    )
                wire.soul_side.send(request)

            async def _ui_loop_fn(wire: Wire) -> None:
                wire_future.set_result(wire.ui_side(merge=merge_wire_messages))
                assert self._runtime.root_wire_hub is not None
                assert self._runtime.approval_runtime is not None
                root_hub_queue = self._runtime.root_wire_hub.subscribe()
                stop_task = asyncio.create_task(stop_ui_loop.wait())
                queue_task = asyncio.create_task(root_hub_queue.get())
                try:
                    for pending in self._runtime.approval_runtime.list_pending():
                        _forward_approval_request(
                            wire,
                            ApprovalRequest(
                                id=pending.id,
                                tool_call_id=pending.tool_call_id,
                                sender=pending.sender,
                                action=pending.action,
                                description=pending.description,
                                display=pending.display,
                                source_kind=pending.source.kind,
                                source_id=pending.source.id,
                                agent_id=pending.source.agent_id,
                                subagent_type=pending.source.subagent_type,
                            ),
                        )
                    while True:
                        done, _ = await asyncio.wait(
                            [stop_task, queue_task],
                            return_when=asyncio.FIRST_COMPLETED,
                        )
                        if stop_task in done:
                            break
                        try:
                            msg = queue_task.result()
                        except QueueShutDown:
                            break
                        match msg:
                            case ApprovalRequest() as request:
                                _forward_approval_request(wire, request)
                                queue_task = asyncio.create_task(root_hub_queue.get())
                                continue
                            case ApprovalResponse() as response:
                                if (
                                    request := forwarded_approval_requests.get(response.request_id)
                                ) and not request.resolved:
                                    request.resolve(response.response, response.feedback)
                            case _:
                                pass
                        wire.soul_side.send(msg)
                        queue_task = asyncio.create_task(root_hub_queue.get())
                finally:
                    stop_task.cancel()
                    queue_task.cancel()
                    with contextlib.suppress(asyncio.CancelledError):
                        await stop_task
                    with contextlib.suppress(asyncio.CancelledError):
                        await queue_task
                    for task in list(approval_bridge_tasks.values()):
                        task.cancel()
                    for task in list(approval_bridge_tasks.values()):
                        with contextlib.suppress(asyncio.CancelledError):
                            await task
                    approval_bridge_tasks.clear()
                    forwarded_approval_requests.clear()
                    assert self._runtime.root_wire_hub is not None
                    self._runtime.root_wire_hub.unsubscribe(root_hub_queue)

            run_cancel_event = asyncio.Event()

            async def _mirror_external_cancel() -> None:
                await cancel_event.wait()
                run_cancel_event.set()

            external_cancel_task = asyncio.create_task(
                _mirror_external_cancel(),
                name="cancel-event-mirror",
            )
            soul_task = asyncio.create_task(
                run_soul(
                    self.soul,
                    user_input,
                    _ui_loop_fn,
                    run_cancel_event,
                    self._runtime.session.wire_file,
                    runtime=self._runtime,
                )
            )

            wire_shut_down = False
            try:
                wire_ui = await wire_future
                while True:
                    msg = await wire_ui.receive()
                    yield msg
            except QueueShutDown:
                wire_shut_down = True
                pass
            finally:
                # stop consuming Wire messages
                stop_ui_loop.set()
                cleanup_cancelled_run = False
                if not wire_shut_down and not soul_task.done() and not cancel_event.is_set():
                    cleanup_cancelled_run = True
                    run_cancel_event.set()
                # wait for the soul task to finish, or raise
                try:
                    await soul_task
                except RunCancelled:
                    if not cleanup_cancelled_run:
                        raise
                finally:
                    external_cancel_task.cancel()
                    with contextlib.suppress(asyncio.CancelledError):
                        await external_cancel_task

    async def run_shell(
        self, command: str | None = None, *, prefill_text: str | None = None
    ) -> bool:
        """Run the Kimi Code CLI instance with shell UI."""
        return await self._run_shell_remote(prefill_text=prefill_text, command=command)

    async def _run_shell_remote(
        self,
        *,
        prefill_text: str | None,
        command: str | None,
    ) -> bool:
        """
        Run the shell UI against the external Wire agent connected in
        `_create_remote` (`self._remote_client`). The agent process owns the
        loop/session/LLM; locally there is only a `RemoteSoul` shim.
        """
        from kimi_cli.ui.shell import Shell, WelcomeInfoItem

        client = self._remote_client
        assert client is not None
        server = client.server_info.get("name", "external agent")
        version = client.server_info.get("version", "")
        welcome_info = [
            WelcomeInfoItem(
                name="Directory", value=str(shorten_home(self._runtime.session.work_dir))
            ),
            WelcomeInfoItem(
                name="Agent",
                value=f"{server} {version}".strip() + " (remote / wire)",
                level=WelcomeInfoItem.Level.INFO,
            ),
        ]
        try:
            async with self._env():
                shell = Shell(
                    self._soul,
                    welcome_info=welcome_info,
                    prefill_text=prefill_text,
                    show_thinking_stream=self._runtime.config.show_thinking_stream,
                )
                return await shell.run(command)
        finally:
            await client.close()

    async def run_print(
        self,
        input_format: InputFormat,
        output_format: OutputFormat,
        command: str | None = None,
        *,
        final_only: bool = False,
    ) -> int:
        """Run the Kimi Code CLI instance with print UI against the remote agent."""
        from kimi_cli.soul.remote import run_remote_soul
        from kimi_cli.ui.print.visualize import visualize
        from kimi_cli.wire.client import WireClientError

        client = self._remote_client
        if client is None:
            raise RuntimeError("Print UI requires a remote agent (KIMI_AGENT_BIN).")

        if command is None:
            if not sys.stdin.isatty() and input_format == "text":
                command = sys.stdin.read().strip()
                logger.info("Read command from stdin: {command}", command=command)
            if not command:
                return 0

        if input_format != "text":
            raise RuntimeError(
                f"Print input format {input_format!r} is not supported in remote mode."
            )

        cancel_event = asyncio.Event()

        def _handler() -> None:
            logger.debug("SIGINT received.")
            cancel_event.set()

        loop = asyncio.get_running_loop()
        remove_sigint = install_sigint_handler(loop, _handler)

        try:
            await run_remote_soul(
                client,
                command,
                lambda wire: visualize(output_format, final_only, wire),
                cancel_event,
            )
            return 0
        except RunCancelled:
            return 1
        except WireClientError as e:
            logger.error("Agent connection error: {error}", error=e)
            return 1
        finally:
            remove_sigint()

    async def run_wire_stdio(self) -> None:
        """Run the Kimi Code CLI instance as Wire server over stdio."""
        from kimi_cli.wire.server import WireServer

        async with self._env():
            server = WireServer(self._soul)
            await server.serve()
