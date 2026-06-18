"""
A tiny deterministic Wire server for validating `WireClient` offline.

Speaks the Wire JSON-RPC protocol over stdio (one JSON object per line), the
same contract as ``kimi --wire`` and the Rust ``kimi-agent``, but with canned
behavior and no LLM/network. Used by ``scripts/wire_bridge_smoke.py``.

Protocol exercised:
  - ``initialize``                       -> handshake response
  - ``prompt``                           -> stream events, raise an approval
                                            request mid-turn, then finish
  - the approval response (a JSON-RPC result) is read back from stdin

Run as a subprocess; it processes one request line at a time on stdin and is
intentionally synchronous: there is only ever one in-flight turn, so blocking on
stdin to read the approval response is correct and keeps the contract obvious.
"""

from __future__ import annotations

import json
import sys

# Import the real codec so emitted bytes are guaranteed wire-correct.
from kosong.message import TextPart
from kosong.tooling import ToolResult, ToolReturnValue

from kimi_cli.wire.protocol import WIRE_PROTOCOL_VERSION
from kimi_cli.wire.serde import serialize_wire_message
from kimi_cli.wire.types import (
    ApprovalRequest,
    StepBegin,
    TurnBegin,
    TurnEnd,
)


def _write(obj: dict) -> None:
    sys.stdout.write(json.dumps(obj, separators=(",", ":")) + "\n")
    sys.stdout.flush()


def _emit_event(msg) -> None:
    _write({"jsonrpc": "2.0", "method": "event", "params": serialize_wire_message(msg)})


def _emit_request(rpc_id: str, msg) -> None:
    _write(
        {"jsonrpc": "2.0", "method": "request", "id": rpc_id, "params": serialize_wire_message(msg)}
    )


def _read_line() -> dict | None:
    line = sys.stdin.readline()
    if not line:
        return None
    line = line.strip()
    if not line:
        return {}
    return json.loads(line)


def _handle_initialize(msg: dict) -> None:
    _write(
        {
            "jsonrpc": "2.0",
            "id": msg["id"],
            "result": {
                "protocol_version": WIRE_PROTOCOL_VERSION,
                "server": {"name": "fake-wire-server", "version": "0.0.0"},
                "slash_commands": [
                    {"name": "help", "description": "Show help", "aliases": []},
                ],
                "capabilities": {"supports_question": True},
            },
        }
    )


def _handle_replay(msg: dict) -> None:
    # Re-emit a canned "prior turn" the way a real agent replays wire.jsonl.
    _emit_event(TurnBegin(user_input="previous question"))
    _emit_event(TextPart(text="(replayed) earlier answer"))
    _emit_event(TurnEnd())
    _write(
        {
            "jsonrpc": "2.0",
            "id": msg["id"],
            "result": {"status": "finished", "events": 3, "requests": 0},
        }
    )


def _handle_prompt(msg: dict) -> None:
    user_input = msg.get("params", {}).get("user_input", "")

    _emit_event(TurnBegin(user_input=user_input))
    _emit_event(StepBegin(n=1))
    _emit_event(TextPart(text="Hello from the fake core. "))
    _emit_event(TextPart(text="Streaming and the bridge both work.\n"))

    # Raise an approval request, then block until the client relays the answer.
    _emit_request(
        "appr-1",
        ApprovalRequest(
            id="appr-1",
            tool_call_id="tc-1",
            sender="Shell",
            action="run shell command",
            description="Run command `ls`",
        ),
    )
    approved = False
    while True:
        reply = _read_line()
        if reply is None:
            return  # stdin closed
        if reply.get("method") == "cancel":
            _write({"jsonrpc": "2.0", "id": reply["id"], "result": {}})
            _write({"jsonrpc": "2.0", "id": msg["id"], "result": {"status": "cancelled"}})
            return
        if "result" in reply and reply.get("result", {}).get("request_id") == "appr-1":
            approved = reply["result"].get("response") in ("approve", "approve_for_session")
            break

    output = "file1.txt\nfile2.txt" if approved else "(command rejected by user)"
    _emit_event(
        ToolResult(
            tool_call_id="tc-1",
            return_value=ToolReturnValue(
                is_error=not approved,
                output=output,
                message="listed directory" if approved else "rejected",
                display=[],
            ),
        )
    )
    _emit_event(TextPart(text=f"\nResult was: {output}"))
    _emit_event(TurnEnd())

    _write({"jsonrpc": "2.0", "id": msg["id"], "result": {"status": "finished"}})


def main() -> None:
    while True:
        msg = _read_line()
        if msg is None:
            break
        if not msg:
            continue
        method = msg.get("method")
        if method == "initialize":
            _handle_initialize(msg)
        elif method == "prompt":
            _handle_prompt(msg)
        elif method == "replay":
            _handle_replay(msg)
        elif method == "cancel":
            # No active turn; ack.
            _write({"jsonrpc": "2.0", "id": msg.get("id"), "result": {}})
        else:
            # Unknown method.
            if msg.get("id") is not None:
                _write(
                    {
                        "jsonrpc": "2.0",
                        "id": msg["id"],
                        "error": {"code": -32601, "message": f"method not found: {method}"},
                    }
                )


if __name__ == "__main__":
    main()
