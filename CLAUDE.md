# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

This repo vendors two subtrees, each with its own toolchain. **Rust** (`core/`) uses cargo; **Python** (`cli/`) uses `uv` + a `Makefile`. Run cargo commands from `core/`, make/uv commands from `cli/`.

### Rust (`core/`) — the agent core
- Build the agent binary: `cargo build -p kimi-agent`
- Build the native GUI frontend: `cargo build -p kimi-gui` (egui shell speaking the wire protocol; run with `--agent-bin <path-to-kimi-agent>` or `KIMI_AGENT_BIN`, remaining args are forwarded to the agent)
- Test a crate: `cargo test -p kimi-agent` (also `-p kosong`, `-p kaos`); whole workspace: `cargo test`
- Single test: `cargo test -p kimi-agent <test_name>`
- Format / lint: `cargo fmt` · `cargo clippy --workspace --all-targets` (workspace denies `unsafe_code`, warns on all clippy)

### Python (`cli/`) — the shell UI
- One-time setup: `make prepare` (syncs `uv` deps for all workspace packages, installs prek hooks)
- Lint + typecheck all packages: `make check` (ruff + pyright + non-blocking ty)
- Format: `make format`
- Test all suites: `make test`; just the CLI: `make test-kimi-cli`
- Single test: `uv run pytest tests/path/to/test_x.py::test_name -vv`
- The `cli/` workspace holds sub-packages (`packages/kosong`, `packages/kaos`, `sdks/kimi-sdk`); run their pytest via `uv run --project <path> --directory <path> pytest ...` (see `cli/Makefile`).

### Running the app (shell UI on the Rust core)
```sh
cd core && cargo build -p kimi-agent && cd ..
cd cli
PYTHONPATH=src KIMI_AGENT_BIN="../core/target/debug/kimi-agent" python -m kimi_cli
```
On Windows (PowerShell) quote the binary path and set `PYTHONUTF8=1`. The banner shows `(remote / wire)` when running on the Rust core. Without `KIMI_AGENT_BIN` the Python side has no working agent loop (see below).

### Python↔Rust E2E
Build the binary, then point the E2E suite at it:
`KIMI_E2E_WIRE_CMD=../target/debug/kimi-agent uv run pytest tests_e2e`

## Sync contract (must-follow)

Rust and Python must stay in lockstep on all external behavior: the wire protocol/envelopes/error codes, `kosong.message` & `kimi_cli.wire.types` schemas and serde, config/session/context JSONL formats under `~/.kimi`, tool schemas/descriptions/approvals, prompts, and compaction. Tool identifiers remain `kimi_cli.tools.*` and wire identity stays "Kimi Code CLI" even in the Rust binary. **The Rust workspace version must exactly match the Python `kimi-cli` version.** When behavior conflicts, Python (`cli/src/kimi_cli`, `cli/packages/*`) and `cli/docs/zh/` are the source of truth. See `core/AGENTS.md` for the full contract.

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
