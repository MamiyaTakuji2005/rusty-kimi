# rusty-kimi

A self-contained coding agent: a **Rust agent core** with a **native Rust GUI**,
connected over the **Wire protocol** (JSON-RPC over stdio).

The Rust side is a complete, standalone system. The agent loop, LLM calls, tools,
context management, skills, and MCP all live in Rust, and the desktop GUI is Rust
too — there is no Python in the runtime path.

This repository also vendors the original **Python shell UI** (`cli/`). It is kept
for protocol compatibility and as a reference, but it is no longer the focus of
development.

## The Rust core is a standalone system

**`kimi-agent`** (`core/kimi-agent/`) is a full agent server — a wire-only process
with no UI of its own. It owns everything that does actual work:

- the agent loop and step orchestration
- LLM provider calls (Kimi, OpenAI-compatible / `openai_legacy`, Echo variants)
- context management and auto-compaction
- all built-in tools (Shell, file Read/Write/Replace/Glob/Grep/ReadMedia, web
  search/fetch, todo, subagents/fork, dmail, think)
- skills and flows
- MCP client integration
- session persistence under `~/.kimi`

**`kimi-gui`** (`core/kimi-gui/`) is the new native frontend (egui/eframe). It
launches and drives one or more `kimi-agent` subprocesses:

- multiple sessions in tabs, each backed by its own agent
- a `+` button opens a native folder picker to start a session in a directory
- a resume menu lists past sessions found under `~/.kimi`
- a live status bar (context usage, YOLO), approval prompts, and the streamed
  transcript

```
kimi-gui (core/, Rust)  ──Wire JSON-RPC / stdio──▶  kimi-agent (core/, Rust)
```

## The Wire protocol is still the contract

The GUI and the agent do not share an in-process API — they speak the **Wire
protocol**: JSON-RPC messages over the agent's stdio. The GUI sends `initialize`,
`prompt`, `cancel`, `replay`, and `steer`, plus approval replies; the agent streams
back typed events (`TurnBegin`, `StepBegin`, `ContentPart`, `ToolResult`,
`StatusUpdate`, `TurnEnd`, …).

Keeping the agent wire-only is deliberate. The Python TUI speaks the same protocol,
so it remains a compatible (if deprecated) frontend, and any other client — or a
future frontend — can attach the same way. The protocol, the `~/.kimi` data layout,
and the `kimi_cli.tools.*` identity stay stable regardless of which frontend is used.

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

| Path    | What it is | Origin |
|---------|------------|--------|
| `core/` | Rust workspace: `kimi-agent`, `kosong`, `kaos`, `kimi-gui` | fork of [MoonshotAI/kimi-agent-rs](https://github.com/MoonshotAI/kimi-agent-rs) + the new `kimi-gui` |
| `cli/`  | Original Python shell UI (TUI) — deprecated, kept for protocol compatibility | fork of [MoonshotAI/kimi-cli](https://github.com/MoonshotAI/kimi-cli) |

Within `core/`:

- `kimi-agent/` — the agent server (bin: `kimi-agent`), wire-only.
- `kosong/` — LLM abstraction (messages, tooling, providers).
- `kaos/` — OS abstraction (paths, stats, filesystem).
- `kimi-gui/` — native egui frontend for the wire protocol (bin: `kimi-gui`).

## The Python TUI (`cli/`)

The fork originally set out to reconcile the Rust core with the newer Python shell
UI. That work has been **deprioritized**: the Python TUI is no longer being
rehabilitated or cleaned up. It stays in-tree because it shares the Wire protocol and
the `~/.kimi` data layout, so it can still drive the Rust agent as a fallback
frontend — but the native Rust GUI is where development effort now goes.

## Build & test

Rust workspace (from `core/`):

```sh
cargo build -p kimi-agent   # the agent server
cargo build -p kimi-gui     # the GUI
cargo test                  # whole workspace
cargo fmt
cargo clippy --workspace --all-targets
```

Python (from `cli/`, optional — see `cli/Makefile`): `make prepare` then `make check`.

## License & attribution

Apache License 2.0 — see [LICENSE](LICENSE) and [NOTICE](NOTICE). Both subtrees are
modified forks of Moonshot AI projects; per-subtree git history is preserved. This is
an independent fork, not affiliated with or endorsed by Moonshot AI; "Kimi"/"Moonshot"
are trademarks of their owners.
