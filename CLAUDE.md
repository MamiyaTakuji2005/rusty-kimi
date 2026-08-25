# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

Single toolchain: Rust, run from `core/`.

### Build
- Agent binary: `cargo build -p kimi-agent`
- GUI frontend: `cargo build -p kimi-gui` (run with `--agent-bin <path-to-kimi-agent>` or `KIMI_AGENT_BIN`; remaining args are forwarded to the agent verbatim)

### Test / lint
- Single test: `cargo test -p kimi-agent <test_name>` (also `-p kosong`, `-p kaos`, `-p kimi-gui`)
- Whole workspace: `cargo test`
- Format / lint: `cargo fmt` · `cargo clippy --workspace --all-targets` (workspace denies `unsafe_code`, warns on all clippy)

## Architecture

Two processes, one language. **`kimi-gui`** (egui frontend, the canonical client) spawns and drives one or more **`kimi-agent`** subprocesses (wire-only agent server) over the **Wire protocol** — JSON-RPC over stdio.

**`kimi-agent`** (`core/kimi-agent/`) owns the agent loop, LLM calls, context management, compaction, tool dispatch, all built-in tools (WriteFile, ReadFile, Glob, Grep, StrReplaceFile, Shell, Undo, …), skills/flows, MCP, and session persistence under `~/.kimi`.

**`kimi-gui`** (`core/kimi-gui/`) owns sessions-as-tabs, themes, the command palette, approval prompts, and transcript rendering. It never executes agent logic.

**Wire protocol** — the IPC seam and the project's stability contract. The GUI sends `initialize`/`prompt`/`cancel`/`replay`/`steer` plus approval replies; the agent streams back typed events (TurnBegin, StepBegin, ContentPart, ToolResult, StatusUpdate, TurnEnd, …).

### History

The repo began as a reconciliation of an older Rust core (fork of MoonshotAI/kimi-agent-rs) with a newer Python shell UI (fork of MoonshotAI/kimi-cli, vendored under `cli/`). All functionality was ported to Rust; the Python TUI was reduced to a pure frontend and then **archived to a separate private repo (`rusty-kimi-tui`) and removed from this tree**. This repo is Rust-only. Do not resurrect Python code from history — `git log` has it if ever truly needed.

## Compatibility contract (must-follow)

With the Python side gone, these are no longer "match Python" rules — they are the invariants that keep existing `~/.kimi` data readable and any wire-protocol client (including the archived TUI) working against the agent:

- Wire protocol envelopes, `type` strings, and error codes stay stable.
- `~/.kimi` config/session/context/wire JSONL formats stay stable.
- Tool identifiers remain `kimi_cli.tools.*`; wire identity stays "Kimi Code CLI" / `KimiCLI/<VERSION>` even in the Rust binary.
- `kosong::message` serde rules (e.g. single-`TextPart` `Message.content` → JSON string) stay stable.

Breaking any of these needs a deliberate protocol version bump, not a casual edit. The Rust workspace owns its own version number now (it is no longer tied to any Python release).

## Workspace layout

- `core/kimi-agent/` — agent server (bin: `kimi-agent`)
- `core/kosong/` — LLM abstraction (messages, tooling, providers)
- `core/kaos/` — OS abstraction (paths, stats, filesystem)
- `core/kimi-gui/` — egui frontend (bin: `kimi-gui`)

See `core/AGENTS.md` and the sub-tree `AGENTS.md` files for module-level detail.
