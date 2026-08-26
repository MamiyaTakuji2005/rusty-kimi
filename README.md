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

# from any local terminal (no -w needed: the daemon supplies its own):
./kimi-tui --remote 127.0.0.1:9000
```

**One config file, both roles.** `~/.kimi/bridge.toml` describes how a
machine serves (`[serve]`) and which remotes it can reach (`[[remotes]]`).
Each half is read only by the side that needs it, and the file is separate
from `config.toml` on purpose — the agent rewrites that one and would drop
sections it does not know:

```toml
# On the VPS: `kimi-bridge remote` with no arguments then does the right
# thing, which is what the systemd unit below runs.
[serve]
listen = "127.0.0.1:9000"
work_dir = "/home/kimi"       # default for sessions that pass no -w

# On your machine: the remotes the frontends can reach.
[[remotes]]
name = "vps"
endpoint = "127.0.0.1:9000"                            # this end of the tunnel
tunnel = "ssh -N -L 9000:127.0.0.1:9000 user@vps"      # optional, see below
default = true
```

`--remote` then takes a **name or a `host:port`**: `kimi-gui --remote vps`,
or `--remote 127.0.0.1:9000` with no config at all. Flags beat the config
file, which beats the built-in defaults.

**The connect button.** With a remote configured, kimi-gui grows a third
button in the tab strip, between the resume and theme buttons:

| light  | meaning                                            | click            |
| ------ | -------------------------------------------------- | ---------------- |
| grey   | not connected                                      | connect          |
| yellow | tunnel up (or starting), daemon not answering yet  | retry now        |
| green  | the daemon answered a `version` probe              | new remote session |

Right-click disconnects and stops the tunnel. If the remote has a `tunnel`
command, the button runs it as a child process and kills it on disconnect —
so the ssh terminal you used to keep open is no longer needed. That command
must be **non-interactive**: it gets no console, so key auth and a known host
are required (`-o BatchMode=yes` is a good idea), and whatever ssh complains
about shows up in the button's tooltip.

Sessions are per-tab: local and remote tabs live side by side, `+` opens
another session on the active tab's machine, and the resume menu lists the
sessions of whichever machine you are looking at.

`--remote` (or `$KIMI_REMOTE`) says where the **first** session opens; after
that the connect button opens more. Agent arguments like `-w` resolve on the
remote machine, and each session gets its own agent and its own connection.
`kimi-bridge local --upstream <addr>` is an optional extra hop for when the
frontends shouldn't know where the upstream lives. Design record:
[`remote/PLAN.md`](remote/PLAN.md).

**Work directory.** A session that names no `-w` gets the daemon's default —
the remote user's home directory, or whatever `[serve] work_dir` /
`--work-dir` says. That is the point of the default: a frontend on another OS
has no way to name a path that exists over there, so it doesn't have to. Pass
`-w /some/remote/path` to override per session; the resume menu always carries
each session's own directory.

**Running it as a service.** `kimi-agent` reads its credentials from the
environment (`KIMI_API_KEY`, …) and its config and sessions from `~/.kimi`, so
a systemd unit has to supply both — with no `HOME`, `~/.kimi` resolves
relative to the working directory and the agent will not find its config:

```ini
[Service]
Environment=HOME=/home/kimi
EnvironmentFile=/home/kimi/.config/kimi-bridge.env   # KIMI_API_KEY=…
WorkingDirectory=/home/kimi
ExecStart=/usr/local/bin/kimi-bridge remote      # reads [serve] from bridge.toml
Restart=on-failure
User=kimi
```

Keep the listen address on loopback. Anything that can reach that port can run
commands as this user — the wire carries shell approvals, and the connecting
frontend is what answers them. Use a dedicated unprivileged user, and let ssh
be the only way in.

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
