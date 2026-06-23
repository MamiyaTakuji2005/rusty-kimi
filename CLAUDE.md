# rusty-kimi

## Architecture

Two-tier system: Python CLI (TUI + wire protocol server) and Rust agent (LLM execution engine).

**Python** (`cli/`) — session management, TUI, slash commands, hooks, MCP loading, approval UI, and wire protocol. It does NOT execute LLM steps itself.

**Rust** (`core/kimi-agent/`) — owns the agent loop, LLM calls, context management, compaction, tool dispatch, and all built-in tool implementations (WriteFile, ReadFile, Glob, Grep, StrReplaceFile, Shell, etc.).

**Wire protocol** — the IPC channel between Python and Rust. Python sends user input; Rust sends back streamed events (TurnBegin, StepBegin, ContentPart, ToolResult, TurnEnd, etc.).

## Dead Code in Python

The following are intentionally dead and must not be restored or modified:

- **`KimiSoul._step()`** (`cli/src/kimi_cli/soul/kimisoul.py`) — raises `RuntimeError("Local LLM execution is disabled; run via the Rust agent (KIMI_AGENT_BIN).")` immediately. All code after that raise is unreachable dead code.

- **`KimiSoul.compact_context()`** — same: raises the same RuntimeError immediately. Dead code follows.

- **`KimiSoul._bind_plan_mode_tools()`** — explicit no-op stub. Comment says "Python-side tool implementations have been removed."

- **Python-side tool loading** (`KimiToolset.load_tools`) — any tool path under `kimi_cli.tools.` is silently skipped. Comment: "Python-side tool implementations have been removed; execution is delegated to the Rust agent over the wire."

The Python kosong chat provider implementations (kimi.py, openai_compatible.py, contrib providers) exist as library code but are not used in the main execution path — the Rust agent owns the LLM call directly.

## Python Cannot Run Standalone

Python has no working agent loop. `KimiSoul._agent_loop()` calls `_step()` which immediately raises. The process requires `KIMI_AGENT_BIN` (the Rust binary) to function. Python's job is:

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
