# rusty-kimi

## Architecture

Two-tier system: Python CLI (TUI + wire protocol server) and Rust agent (LLM execution engine).

**Python** (`cli/`) — session management, TUI, slash commands, hooks, MCP loading, approval UI, and wire protocol. It does NOT execute LLM steps itself.

**Rust** (`core/kimi-agent/`) — owns the agent loop, LLM calls, context management, compaction, tool dispatch, and all built-in tool implementations (WriteFile, ReadFile, Glob, Grep, StrReplaceFile, Shell, etc.).

**Wire protocol** — the IPC channel between Python and Rust. Python sends user input; Rust sends back streamed events (TurnBegin, StepBegin, ContentPart, ToolResult, TurnEnd, etc.).

## Vestigial Local-Execution Code in Python

Local LLM execution was removed; the Rust agent (`KIMI_AGENT_BIN`) owns it. The Python remnants are intentionally minimal — do not rebuild a local agent loop on top of them:

- **`KimiSoul`** (`cli/src/kimi_cli/soul/kimisoul.py`) — reduced to a stub: a placeholder class plus the `SKILL_COMMAND_PREFIX` / `FLOW_COMMAND_PREFIX` constants. It exists only so `isinstance(soul, KimiSoul)` checks in the shell resolve — always `False` in remote mode, where the active soul is `RemoteSoul`. Constructing one raises `RuntimeError`.

- **`soul/compaction.py`** — reduced to the `estimate_text_tokens` helper used by the shim; the local `SimpleCompaction` is gone (Rust owns compaction).

- **Python-side tool loading** (`KimiToolset.load_tools`) — any tool path under `kimi_cli.tools.` is silently skipped; execution is delegated to the Rust agent over the wire.

Removed entirely (do not re-add): `soul/slash.py` (local slash registry), `soul/btw.py`, and the `skill/`, `prompts/`, `agents/`, `skills/` package data — all owned by the Rust agent now.

The Python kosong chat provider implementations exist as library code but are not used in the main execution path — the Rust agent owns the LLM call directly.

## Python Cannot Run Standalone

Python has no agent loop. In remote mode the active soul is `RemoteSoul`, a thin shim over the wire connection; `KimiSoul` is a stub that raises on construction. The process requires `KIMI_AGENT_BIN` (the Rust binary) to function. Python's job is:

1. Start the Rust agent subprocess
2. Manage the session, TUI, approval prompts, hooks, MCP OAuth
3. Bridge user input and Rust output over the wire protocol

## What Belongs Where

| Concern | Python | Rust |
|---|---|---|
| TUI / shell prompt | ✓ | |
| Session state | ✓ | |
| Slash commands | ✓ | |
| Hook engine | ✓ | |
| MCP OAuth | ✓ | |
| Wire protocol framing | both | both |
| LLM API calls | | ✓ |
| Agent loop / step | | ✓ |
| Context compaction | | ✓ |
| Tool dispatch | | ✓ |
| Built-in tools | | ✓ |
| MCP tool calls | both (Python manages client) | ✓ (dispatches) |
