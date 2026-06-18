"""
Focused test for the `steer` wire method (mid-turn user injection).

Checks:
  1. steer with no turn in progress -> error (INVALID_STATE)
  2. steer DURING a multi-step turn -> agent emits a SteerInput event and
     injects the message (best-effort: also looks for the steered keyword in
     the final output).

Usage:
    PYTHONPATH=src python scripts/wire_steer_smoke.py -- <agent> --wire ...
"""

from __future__ import annotations

import asyncio
import os
import sys

from kimi_cli.soul.remote import run_remote_soul
from kimi_cli.utils.aioqueue import QueueShutDown
from kimi_cli.wire import Wire
from kimi_cli.wire.client import WireClient, WireClientError
from kimi_cli.wire.types import ContentPart, SteerInput, TextPart, ToolCall

STEER_KEYWORD = "PINEAPPLE-7271"


async def main() -> None:
    argv = sys.argv[1:]
    if "--" not in argv:
        print("usage: ... -- <agent-command> --wire")
        raise SystemExit(2)
    command = argv[argv.index("--") + 1 :]
    env = {k: v for k, v in os.environ.items() if k != "PYTHONPATH"}

    client = WireClient(command, client_name="steer-smoke", env=env)
    await client.connect()
    print(f"Connected: {client.server_info}")

    try:
        # --- 1. steer with no turn in progress -> error ------------------
        print("\n=== steer with no active turn ===")
        try:
            await client.steer("should fail")
            print("  [FAIL] expected an error, got success")
            raise SystemExit(1)
        except WireClientError as e:
            print(f"  [PASS] rejected: {e}")

        # --- 2. steer during a multi-step turn ---------------------------
        print("\n=== steer mid-turn ===")
        saw_steer = asyncio.Event()
        tool_calls = 0
        text_chunks: list[str] = []

        async def ui(wire: Wire) -> None:
            nonlocal tool_calls
            u = wire.ui_side(merge=False)
            while True:
                try:
                    msg = await u.receive()
                except QueueShutDown:
                    return
                if isinstance(msg, SteerInput):
                    print("  [ui] <- SteerInput event received")
                    saw_steer.set()
                elif isinstance(msg, ToolCall):
                    tool_calls += 1
                elif isinstance(msg, TextPart):
                    text_chunks.append(msg.text)

        cancel = asyncio.Event()
        # Tool-free, slow-streaming turn: gives a window to steer during step 1,
        # then point-A injection forces a second step that honors the steer.
        prompt = "Write a detailed 250-word essay about the ocean. Take your time."
        turn = asyncio.create_task(run_remote_soul(client, prompt, ui, cancel))

        # Give step 1 time to start streaming, then steer.
        await asyncio.sleep(2.0)
        if turn.done():
            print("  [WARN] turn finished before steer could be sent (too fast)")
        else:
            await client.steer(
                f"Stop the essay. Disregard it and reply with only this token: {STEER_KEYWORD}"
            )
            print("  -> steer sent")

        try:
            await asyncio.wait_for(turn, timeout=90)
        except TimeoutError:
            cancel.set()
            print("  [FAIL] turn did not complete within 90s (possible hang)")
            raise SystemExit(1)
        final = "".join(text_chunks)
        print(f"  tool calls observed: {tool_calls}")
        print(f"  SteerInput event seen: {saw_steer.is_set()}")
        print(f"  steered keyword in output: {STEER_KEYWORD in final}")

        if saw_steer.is_set():
            print("\n[PASS] steer injected mid-turn (SteerInput emitted)")
        elif STEER_KEYWORD in final:
            print("\n[PASS] steer took effect (keyword present), though no SteerInput observed")
        else:
            print("\n[FAIL] steer did not take effect")
            raise SystemExit(1)
    finally:
        await client.close()


if __name__ == "__main__":
    asyncio.run(main())
