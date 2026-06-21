from __future__ import annotations

import asyncio
import contextlib
from typing import TYPE_CHECKING, Any

from kosong.message import ContentPart

from kimi_cli.soul import (  # pyright: ignore[reportPrivateUsage]
    RunCancelled,
    StatusSnapshot,
    UILoopFn,
    _current_wire,
)
from kimi_cli.utils.aioqueue import QueueShutDown
from kimi_cli.utils.logging import logger
from kimi_cli.wire import Wire
from kimi_cli.wire.client import WireClient

if TYPE_CHECKING:
    from kimi_cli.llm import ModelCapability


async def run_remote_soul(
    client: WireClient,
    user_input: str | list[ContentPart],
    ui_loop_fn: UILoopFn,
    cancel_event: asyncio.Event,
) -> str:
    """
    Drop-in mirror of `kimi_cli.soul.run_soul`, but instead of running an
    in-process `Soul`, it drives an external Wire agent through `client`.

    A fresh `Wire` is created per turn and the UI loop is attached to it exactly
    as in `run_soul`; the only difference is that the producer of `WireMessage`s
    is the agent subprocess (via `WireClient`) rather than `soul.run()`.

    Note: no `WireFile` backend is created here — the agent process owns session
    persistence (it writes its own ``wire.jsonl`` / ``context.jsonl``), so
    recording locally would double-write.

    Returns the turn status: "finished" | "cancelled" | "max_steps_reached".

    Raises:
        RunCancelled: When the run is cancelled by the cancel event.
    """
    wire = Wire()
    wire_token = _current_wire.set(wire)

    logger.debug("Starting UI loop for remote turn")
    ui_task = asyncio.create_task(ui_loop_fn(wire))

    try:
        status = await client.prompt(user_input, wire, cancel_event)
    finally:
        logger.debug("Shutting down remote turn wire")
        wire.shutdown()
        await wire.join()
        try:
            await asyncio.wait_for(ui_task, timeout=0.5)
        except QueueShutDown:
            logger.debug("UI loop shut down")
        except TimeoutError:
            logger.warning("UI loop timed out")
        finally:
            _current_wire.reset(wire_token)

    if status == "cancelled" and cancel_event.is_set():
        raise RunCancelled
    return status


class RemoteSoul:
    """
    A thin `Soul`-shaped shim backed by a `WireClient`.

    It is deliberately *not* a `KimiSoul`: the agent process owns the real loop,
    session, history and LLM, so this object only surfaces the read-only bits the
    shell UI reads directly (`name`, `model_name`, `status`, ...) and routes
    `steer` to the agent. Because `isinstance(soul, KimiSoul)` is False, the
    shell's MCP / background-task / plan-mode paths degrade gracefully.

    `run()` is never called: `Shell.run_soul_command` branches to
    `run_remote_soul` for `RemoteSoul`, so turns go over the wire instead.
    """

    def __init__(self, client: WireClient) -> None:
        self.client = client
        self._hook_engine: Any | None = None

    @property
    def hook_engine(self) -> Any:
        # Hooks run in the agent process; the shell never executes them in remote
        # mode. Provide an empty engine to satisfy the Soul protocol.
        if self._hook_engine is None:
            from kimi_cli.hooks.engine import HookEngine

            self._hook_engine = HookEngine()
        return self._hook_engine

    @property
    def name(self) -> str:
        return str(self.client.server_info.get("name") or "Kimi Code CLI")

    @property
    def model_name(self) -> str:
        # Check StatusUpdate first (updates live after /model)
        s = self.client.last_status
        if s is not None and s.model:
            return s.model
        # Fall back to initialize handshake
        model = self.client.server_info.get("model")
        if model:
            return str(model)
        return str(self.client.server_info.get("name") or "(remote agent)")

    @property
    def model_capabilities(self) -> set[ModelCapability] | None:
        return None

    @property
    def thinking(self) -> bool | None:
        # The agent reports thinking state via StatusUpdate; server_info has no
        # equivalent, so fall back to None until the first status arrives.
        s = self.client.last_status
        if s is not None and s.thinking is not None:
            return s.thinking
        return None

    @property
    def status(self) -> StatusSnapshot:
        s = self.client.last_status
        if s is None:
            return StatusSnapshot(context_usage=0.0)
        return StatusSnapshot(
            context_usage=s.context_usage or 0.0,
            context_tokens=s.context_tokens or 0,
            max_context_tokens=s.max_context_tokens or 0,
            yolo_enabled=bool(s.yolo_enabled),
            plan_mode=bool(s.plan_mode),
        )

    @property
    def available_slash_commands(self) -> list[Any]:
        """Slash commands reported by the remote agent via initialize."""
        from kimi_cli.utils.slashcmd import SlashCommand

        def _noop(*args: Any, **kwargs: Any) -> None:
            pass

        return [
            SlashCommand(
                name=cmd["name"],
                description=cmd.get("description", ""),
                func=_noop,
                aliases=cmd.get("aliases", []),
            )
            for cmd in self.client.slash_commands
        ]

    def steer(self, user_input: str | list[ContentPart]) -> None:
        """Inject follow-up input into the running turn (fire-and-forget)."""
        asyncio.get_event_loop().create_task(self.client.steer(user_input))

    async def run(self, *args: Any, **kwargs: Any) -> None:
        raise RuntimeError("RemoteSoul.run() must not be called; use run_remote_soul().")


async def run_remote_replay(client: WireClient, ui_loop_fn: UILoopFn) -> dict:
    """
    Replay the agent's recorded history into a fresh `Wire` and render it through
    `ui_loop_fn`. The agent re-emits the `event`/`request` messages it recorded in
    its session ``wire.jsonl``; this is how an external UI restores a resumed
    session. Mirror of `run_remote_soul` but read-only (no prompt).

    Returns the replay result dict, e.g. ``{"status": ..., "events": N, "requests": M}``.
    """
    wire = Wire()
    wire_token = _current_wire.set(wire)
    ui_task = asyncio.create_task(ui_loop_fn(wire))
    try:
        return await client.replay(wire)
    finally:
        wire.shutdown()
        await wire.join()
        try:
            await asyncio.wait_for(ui_task, timeout=0.5)
        except (QueueShutDown, TimeoutError):
            pass
        finally:
            _current_wire.reset(wire_token)
