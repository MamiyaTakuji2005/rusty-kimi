# rusty-kimi

A self-contained coding agent: a **Rust agent core** with a **native Rust GUI**,
connected over the **Wire protocol** (JSON-RPC over stdio).

Everything in the runtime path is Rust. The agent loop, LLM calls, tools,
context management, skills, and MCP live in the agent server; the desktop GUI is
Rust too. There is no Python anywhere.

```
kimi-gui (Rust, egui)  ──Wire JSON-RPC / stdio──▶  kimi-agent (Rust)
kimi-tui (Rust, ratatui) ──Wire JSON-RPC / stdio─▶  kimi-agent (Rust)
```

## The agent: `kimi-agent`

**`kimi-agent`** (`server/kimi-agent/`) is a full agent server — a wire-only process
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

**`kimi-gui`** (`client/kimi-gui/`) is the native frontend (egui/eframe) and the
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

> **Terminal frontend:** the original Python TUI that spoke this protocol was
> archived to a separate private repo (`rusty-kimi-tui`). The Rust-native
> replacement now ships in this repo as **`kimi-tui`** (below).

## Quick start

```sh
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

## Remote access: `kimi-bridge`

Run the agent on another machine (a VPS, a beefy build box) and drive it from
your local `kimi-tui` / `kimi-gui`. The bridge is a pair of **dumb byte
relays** — they never parse the wire protocol, so they stay independent of
protocol versions. No auth, no TLS: keep both ends on loopback and cross the
network through an ssh tunnel.

```sh
# on the remote box:
./kimi-bridge remote --listen 127.0.0.1:9000

# on the local box (kept open):
ssh -N -L 9000:127.0.0.1:9000 user@remote

# from any local terminal:
./kimi-tui --remote 127.0.0.1:9000 -w /path/on/remote
```

`--remote` (or `$KIMI_REMOTE`) makes every frontend connect through the
daemon: agent arguments like `-w` resolve **on the remote machine**, and the
resume menu lists the remote `~/.kimi` sessions. One agent per connection.
`kimi-bridge local --upstream <addr>` is an optional extra hop for when the
frontends shouldn't know where the upstream lives. Design record:
[`remote/PLAN.md`](remote/PLAN.md).

## Repo layout

The workspace root is the top-level `Cargo.toml`; run cargo from the repo root.

- `server/kimi-agent/` — the agent server (bin: `kimi-agent`), wire-only.
- `server/kosong/` — LLM abstraction (messages, tooling, providers).
- `server/kaos/` — OS abstraction (paths, stats, filesystem).
- `client/wire-client/` — shared frontend kit: JSON-RPC over stdio, transcript folding, session listing, agent-binary resolution.
- `client/kimi-gui/` — native egui frontend for the wire protocol (bin: `kimi-gui`).
- `client/kimi-tui/` — ratatui terminal frontend (bin: `kimi-tui`); one session per invocation.
- `remote/kimi-bridge/` — relay daemons for remote access (bin: `kimi-bridge`).

## Build & test

Rust workspace (from the repo root):

```sh
cargo build -p kimi-agent   # the agent server
cargo build -p kimi-gui     # the GUI
cargo build -p kimi-tui     # the terminal UI
cargo build -p kimi-bridge  # the remote relay daemons
cargo test                  # whole workspace
cargo fmt
cargo clippy --workspace --all-targets
```

Run the TUI from a real terminal — it takes over the screen and restores it on exit:

```sh
./target/debug/kimi-tui -w /some/dir        # start a session in a directory
# Enter sends · Esc cancels the turn · 1/2/3 answer approvals · Tab cycles fork views
# PgUp/PgDn or mouse wheel scrolls · Ctrl+O resume menu · Ctrl+C quits
```

## License & attribution

Apache License 2.0 — see [LICENSE](LICENSE) and [NOTICE](NOTICE). The code began
as modified forks of Moonshot AI projects (kimi-agent-rs, and kimi-cli until the
Python side was archived out); per-subtree git history is preserved. This is an
independent fork, not affiliated with or endorsed by Moonshot AI; "Kimi"/"Moonshot"
are trademarks of their owners.
