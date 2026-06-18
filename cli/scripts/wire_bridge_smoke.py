"""
Headless end-to-end smoke test for the Wire bridge.

Drives a real `WireClient` against a subprocess Wire server and runs turns
through `run_remote_soul` with a *headless* UI loop that records every
`WireMessage` and resolves approval requests. This proves the full bridge path
— subprocess spawn, initialize handshake, prompt round-trip, event
deserialization into the in-process `Wire`, approval request relay, response
matching, and multi-turn process persistence — without needing a TTY.

The real shell UI (`kimi_cli.ui.shell.visualize`) is just a different
`ui_loop_fn` that consumes the same `wire.ui_side()`, so a green run here means
the bridge is ready to carry the real UI.

Usage:
    PYTHONPATH=src python scripts/wire_bridge_smoke.py
    PYTHONPATH=src python scripts/wire_bridge_smoke.py -- kimi --wire   # live agent
"""

from __future__ import annotations

import asyncio
import os
import sys

from kosong.message import TextPart

from kimi_cli.soul import RunCancelled
from kimi_cli.soul.remote import run_remote_replay, run_remote_soul
from kimi_cli.utils.aioqueue import QueueShutDown
from kimi_cli.wire import Wire
from kimi_cli.wire.client import WireClient
from kimi_cli.wire.types import ApprovalRequest, QuestionRequest, ToolResult, TurnBegin, TurnEnd

HERE = os.path.dirname(os.path.abspath(__file__))
FAKE_SERVER = os.path.join(HERE, "fake_wire_server.py")


class Recorder:
    """A headless UI loop: records messages, auto-resolves approvals/questions."""

    def __init__(self) -> None:
        self.messages: list[object] = []

    async def ui_loop(self, wire: Wire) -> None:
        ui = wire.ui_side(merge=False)
        while True:
            try:
                msg = await ui.receive()
            except QueueShutDown:
                return
            self.messages.append(msg)
            match msg:
                case ApprovalRequest():
                    print(f"  [ui] approval requested: {msg.description!r} -> approving")
                    msg.resolve("approve")
                case QuestionRequest():
                    answers = {q.question: q.options[0].label for q in msg.questions}
                    msg.resolve(answers)
                case TextPart():
                    print(f"  [ui] text: {msg.text!r}")
                case TurnBegin():
                    print("  [ui] turn begin")
                case TurnEnd():
                    print("  [ui] turn end")
                case ToolResult():
                    print(f"  [ui] tool result: {msg.return_value.output!r}")


class CancelOnApproval:
    """UI loop that, instead of answering an approval, cancels the turn."""

    def __init__(self) -> None:
        self.cancel_event = asyncio.Event()
        self.messages: list[object] = []

    async def ui_loop(self, wire: Wire) -> None:
        ui = wire.ui_side(merge=False)
        while True:
            try:
                msg = await ui.receive()
            except QueueShutDown:
                return
            self.messages.append(msg)
            if isinstance(msg, ApprovalRequest):
                print("  [ui] approval requested -> cancelling turn instead")
                self.cancel_event.set()


def _check(cond: bool, label: str) -> None:
    mark = "PASS" if cond else "FAIL"
    print(f"  [{mark}] {label}")
    if not cond:
        raise SystemExit(f"smoke test failed: {label}")


async def _run_live(client: WireClient) -> None:
    """Reduced checks for a real agent (e.g. ``kimi --wire``): one short turn."""
    print("\n=== Live turn ===")
    rec = Recorder()
    cancel = asyncio.Event()
    status = await run_remote_soul(
        client, "Reply with exactly one word: hello", rec.ui_loop, cancel
    )
    print(f"  status = {status}")
    text = "".join(m.text for m in rec.messages if isinstance(m, TextPart))
    _check(status in ("finished", "max_steps_reached"), "live turn completed")
    _check(bool(text.strip()), "agent streamed some text")
    _check(any(isinstance(m, TurnEnd) for m in rec.messages), "received TurnEnd")
    print(f"  agent said: {text.strip()[:200]!r}")

    # Resume: replay this live session's recorded history back through the UI.
    print("\n=== Live replay (resume) ===")
    rrec = Recorder()
    result = await run_remote_replay(client, rrec.ui_loop)
    print(f"  replay result = {result}")
    if result.get("status") == "unsupported":
        print("  [SKIP] agent does not implement the 'replay' wire method (optional, Wire 1.3+)")
    else:
        _check(result.get("status") == "finished", "live replay finished")
        _check(result.get("events", 0) > 0, "agent re-emitted recorded events")
        _check(
            any(isinstance(m, TurnBegin) for m in rrec.messages),
            "prior turn re-rendered through the UI on resume",
        )
    print("\nLive bridge checks passed.")


async def main() -> None:
    argv = sys.argv[1:]
    if "--" in argv:
        command = argv[argv.index("--") + 1 :]
        live = True
    else:
        command = [sys.executable, FAKE_SERVER]
        live = False

    # For a live agent (its own installed package), don't leak our source tree
    # via PYTHONPATH into the child process.
    child_env = None
    if live:
        child_env = {k: v for k, v in os.environ.items() if k != "PYTHONPATH"}

    print(f"Connecting to wire agent: {' '.join(command)}")
    client = WireClient(
        command, client_name="wire-bridge-smoke", client_version="0.1", env=child_env
    )
    await client.connect()
    print(f"Server info: {client.server_info}  legacy={client.legacy_mode}")

    try:
        if live:
            await _run_live(client)
            return

        # --- Turn 1 -------------------------------------------------------
        print("\n=== Turn 1 ===")
        rec1 = Recorder()
        cancel = asyncio.Event()
        status = await run_remote_soul(client, "list files", rec1.ui_loop, cancel)
        print(f"  status = {status}")

        kinds = [type(m).__name__ for m in rec1.messages]
        _check(status == "finished", "turn finished")
        _check(any(isinstance(m, TurnBegin) for m in rec1.messages), "received TurnBegin")
        _check(
            any(isinstance(m, TextPart) and "fake core" in m.text for m in rec1.messages),
            "received streamed text",
        )
        _check(
            any(isinstance(m, ApprovalRequest) for m in rec1.messages),
            "received ApprovalRequest",
        )
        _check(
            any(isinstance(m, ToolResult) and not m.return_value.is_error for m in rec1.messages),
            "tool ran after approval was relayed back",
        )
        _check(any(isinstance(m, TurnEnd) for m in rec1.messages), "received TurnEnd (tail event)")
        print(f"  message sequence: {kinds}")

        # --- Turn 2 (proves the subprocess persists across turns) ---------
        print("\n=== Turn 2 (same process) ===")
        rec2 = Recorder()
        status2 = await run_remote_soul(client, "again", rec2.ui_loop, asyncio.Event())
        _check(status2 == "finished", "second turn on same process finished")
        _check(any(isinstance(m, TurnEnd) for m in rec2.messages), "second turn completed fully")

        # --- Cancel: don't approve, cancel the turn instead ---------------
        print("\n=== Cancel ===")
        cancelling = CancelOnApproval()
        cancelled = False
        try:
            await run_remote_soul(
                client, "list files", cancelling.ui_loop, cancelling.cancel_event
            )
        except RunCancelled:
            # Mirrors run_soul: a cancelled turn raises, which run_soul_command catches.
            cancelled = True
        _check(cancelled, "cancel relayed and turn reported cancelled (RunCancelled raised)")

        # --- Replay / resume (the actual point: old-session rendering) ----
        print("\n=== Replay (resume) ===")
        rrec = Recorder()
        result = await run_remote_replay(client, rrec.ui_loop)
        print(f"  replay result = {result}")
        _check(result.get("status") == "finished", "replay finished")
        _check(
            any(isinstance(m, TurnBegin) for m in rrec.messages),
            "replay re-emitted prior turn events",
        )
        _check(
            any(isinstance(m, TextPart) and "replayed" in m.text for m in rrec.messages),
            "replayed content rendered through the UI",
        )

        print("\nAll bridge checks passed.")
    finally:
        await client.close()


if __name__ == "__main__":
    asyncio.run(main())
