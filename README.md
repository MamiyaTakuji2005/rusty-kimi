# rusty-kimi

A self-contained coding agent: a **Rust agent core** with a **native Rust GUI**,
connected over the **Wire protocol** (JSON-RPC over stdio).

Everything in the runtime path is Rust. The agent loop, LLM calls, tools,
context management, skills, and MCP live in the agent server; the desktop GUI is
Rust too. There is no Python anywhere.

```
kimi-gui (Rust, egui)  ──Wire JSON-RPC / stdio──▶  kimi-agent (Rust)
```

## The agent: `kimi-agent`

**`kimi-agent`** (`core/kimi-agent/`) is a full agent server — a wire-only process
with no UI of its own. It owns everything that does actual work:

- the agent loop and step orchestration
- LLM provider calls (Kimi, OpenAI-compatible / `openai_legacy`, Echo variants)
- context management and auto-compaction
- all built-in tools (Shell, file Read/Write/Replace/Glob/Grep/ReadMedia, web
  search/fetch, todo, subagents/fork, undo, dmail, think)
- skills and flows
- MCP client integration
- session persistence under `~/.kimi`

## The GUI: `kimi-gui`

**`kimi-gui`** (`core/kimi-gui/`) is the native frontend (egui/eframe) and the
canonical client. It launches and drives one or more `kimi-agent` subprocesses:

- multiple sessions in tabs, each backed by its own agent; forks as sub-tabs
- full keyboard control (`Ctrl+N/O/T/P/D`, `Tab`, `Enter`, `Esc`)
- a command palette for everything without a key of its own
- light / dark / Kimi themes (the shell UI's slate-and-cyan palette, moon-phase
  spinner included)
- a live status bar (context usage, YOLO), approval prompts, and the streamed
  transcript

## The Wire protocol is the contract

The GUI and the agent do not share an in-process API — they speak the **Wire
protocol**: JSON-RPC messages over the agent's stdio. The GUI sends `initialize`,
`prompt`, `cancel`, `replay`, and `steer`, plus approval replies; the agent streams
back typed events (`TurnBegin`, `StepBegin`, `ContentPart`, `ToolResult`,
`StatusUpdate`, `TurnEnd`, …).

Keeping the agent wire-only is deliberate: the protocol is the project's stable
seam. The protocol, the `~/.kimi` data layout, and the `kimi_cli.tools.*` tool
identity are treated as compatibility invariants — any client speaking the
protocol can drive the agent.

> **Terminal frontend:** the original Python TUI that spoke this protocol now
> lives archived and unmaintained in a separate private repo (`rusty-kimi-tui`).
> It still works against a wire-compatible `kimi-agent` for terminal-only
> environments, but nothing develops it. A minimal Rust TUI client would be the
> path back to a terminal frontend, not resurrecting the Python tree.

## Quick start

```sh
cd core
cargo build -p kimi-agent -p kimi-gui
./target/debug/kimi-gui --agent-bin ./target/debug/kimi-agent
```

- `--agent-bin <path>` (or the `KIMI_AGENT_BIN` environment variable) points the GUI
  at the agent binary.
- Any remaining arguments are forwarded to the agent verbatim — e.g. `-w <dir>`,
  `--session <id>`, `--continue`, `--model <name>`.
- Run `kimi-agent` on its own for a headless server with no UI.

> **Windows note:** if a build fails with "Access is denied" replacing
> `kimi-agent.exe`, a running `kimi-gui`/`kimi-agent` is holding the binary — close
> it and rebuild.

## Repo layout

Within `core/`:

- `kimi-agent/` — the agent server (bin: `kimi-agent`), wire-only.
- `kosong/` — LLM abstraction (messages, tooling, providers).
- `kaos/` — OS abstraction (paths, stats, filesystem).
- `wire-client/` — shared frontend client layer: JSON-RPC over stdio, transcript folding, session listing.
- `kimi-gui/` — native egui frontend for the wire protocol (bin: `kimi-gui`).

## Build & test

Rust workspace (from `core/`):

```sh
cargo build -p kimi-agent   # the agent server
cargo build -p kimi-gui     # the GUI
cargo test                  # whole workspace
cargo fmt
cargo clippy --workspace --all-targets
```

## License & attribution

Apache License 2.0 — see [LICENSE](LICENSE) and [NOTICE](NOTICE). The code began
as modified forks of Moonshot AI projects (kimi-agent-rs, and kimi-cli until the
Python side was archived out); per-subtree git history is preserved. This is an
independent fork, not affiliated with or endorsed by Moonshot AI; "Kimi"/"Moonshot"
are trademarks of their owners.
