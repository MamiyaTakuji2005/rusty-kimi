# rusty-kimi

A self-contained coding agent: a **Rust agent core** driven by native Rust
frontends over the **Wire protocol** (JSON-RPC over stdio). Everything in the
runtime path is Rust.

```
kimi-gui (Rust, egui)    ──Wire JSON-RPC / stdio──▶  kimi-agent (Rust)
kimi-tui (Rust, ratatui) ──Wire JSON-RPC / stdio──▶  kimi-agent (Rust)
                  └─(optional)─ kimi-bridge ──TCP (ssh -L)──▶ kimi-agent on a remote box
```

## The pieces

**`kimi-agent`** (`server/kimi-agent/`) is the agent server — wire-only, no UI
of its own. It owns the agent loop, LLM provider calls (Kimi,
OpenAI-compatible, Echo variants for tests), context management and
auto-compaction, the built-in tools (Shell, file Read/Write/Replace/Glob/Grep,
web search/fetch, todo, subagents/fork, undo, dmail, think), skills and flows,
MCP, and session persistence under `~/.kimi`.

**`kimi-gui`** (`client/kimi-gui/`) is the native egui frontend and canonical
client: sessions in tabs (one `kimi-agent` subprocess each, forks as sub-tabs),
full keyboard control, a `Ctrl+P` command palette, light/dark/Kimi themes,
approval prompts, a live status bar, and the streamed transcript.

**`kimi-tui`** (`client/kimi-tui/`) is the ratatui terminal frontend — one
session per invocation. (The original Python TUI is archived in a separate
private repo.)

**The Wire protocol is the contract.** Frontends send `initialize`, `prompt`,
`cancel`, `replay`, and `steer` plus approval replies; the agent streams back
typed events (`TurnBegin`, `ContentPart`, `ToolResult`, `TurnEnd`, …). The
protocol, the `~/.kimi` data layout, and the `kimi_cli.tools.*` tool identity
are compatibility invariants: any client speaking the protocol can drive the
agent.

## Quick start

```sh
cargo build -p kimi-agent -p kimi-gui
./target/debug/kimi-gui --agent-bin ./target/debug/kimi-agent
```

- `--agent-bin <path>` (or `KIMI_AGENT_BIN`) points a frontend at the agent
  binary; remaining arguments are forwarded to the agent verbatim (`-w <dir>`,
  `--session <id>`, `--continue`, `--model <name>`, …).
- Run `kimi-agent` on its own for a headless server.

> **Windows note:** "Access is denied" replacing `kimi-agent.exe` during a
> build means a running `kimi-gui`/`kimi-agent` holds the binary — close it
> and rebuild.

## Remote access: `kimi-bridge`

Run the agent on another machine and drive it from a local frontend. The
bridge is a pair of **dumb byte relays** — they never parse the wire protocol.
No auth, no TLS: keep both ends on loopback and cross the network through an
ssh tunnel.

```sh
# on the remote box:
./kimi-bridge remote --listen 127.0.0.1:9000

# on the local box (kept open, or configured as a `tunnel` below):
ssh -N -L 9000:127.0.0.1:9000 user@remote

# from any local terminal (no -w needed: the daemon supplies its own):
./kimi-tui --remote 127.0.0.1:9000
```

**One config file, both roles.** `~/.kimi/bridge.toml` describes how a machine
serves (`[serve]`) and which remotes it can reach (`[[remotes]]`); each half is
read only by the side that needs it. It is separate from `config.toml` because
the agent rewrites that file and would drop unknown sections.

```toml
# On the VPS: `kimi-bridge remote` with no arguments then does the right thing.
[serve]
listen = "127.0.0.1:9000"
work_dir = "/home/kimi"       # default for sessions that pass no -w

# On your machine: the remotes the frontends can reach.
[[remotes]]
name = "vps"
endpoint = "127.0.0.1:9000"                            # this end of the tunnel
tunnel = "ssh -N -L 9000:127.0.0.1:9000 user@vps"      # optional, run/killed by the GUI
default = true
```

`--remote` takes a **name or a `host:port`** (`kimi-gui --remote vps`, or
`--remote 127.0.0.1:9000` with no config at all) and says where the first
session opens. Flags beat the config file, which beats built-in defaults.

**Chain buttons.** kimi-gui shows one chain button per `[[remotes]]` entry,
between the resume and theme buttons:

| light  | meaning                                            | click              |
| ------ | -------------------------------------------------- | ------------------ |
| grey   | not connected                                      | connect            |
| yellow | tunnel up (or starting), daemon not answering yet  | retry now          |
| green  | the daemon answered a `version` probe              | new remote session |

Right-click disconnects and stops the tunnel. A configured `tunnel` command is
run as a child process and killed on disconnect; it must be non-interactive
(key auth, known host — `-o BatchMode=yes` helps), and its complaints appear in
the button's tooltip.

The palette's remote commands (`connect to remote`, `new remote session`,
`open remote session`) act on the **default** remote (`default = true`, else
the first); append a name — `open remote session vps` — to pick another.

Sessions are per-tab: local and remote tabs live side by side, `+` opens
another session on the active tab's machine, and the resume menu lists the
sessions of whichever machine you are looking at. A session that names no `-w`
gets the daemon's default work directory (`[serve] work_dir`, else the remote
user's home) — a frontend on another OS cannot name a path that exists over
there, so it doesn't have to. Design record: [`remote/PLAN.md`](remote/PLAN.md).

**Running it as a service.** `kimi-agent` reads credentials from the
environment (`KIMI_API_KEY`, …) and config/sessions from `~/.kimi`, so the unit
must supply `HOME`:

```ini
[Service]
Environment=HOME=/home/kimi
EnvironmentFile=/home/kimi/.config/kimi-bridge.env   # KIMI_API_KEY=…
WorkingDirectory=/home/kimi
ExecStart=/usr/local/bin/kimi-bridge remote      # reads [serve] from bridge.toml
Restart=on-failure
User=kimi
```

Keep the listen address on loopback: anything that reaches that port can run
commands as this user. Use a dedicated unprivileged user and let ssh be the
only way in.

## Repo layout

The workspace root is the top-level `Cargo.toml`; run cargo from the repo root.

- `server/kimi-agent/` — the agent server (bin: `kimi-agent`), wire-only.
- `server/kosong/` — LLM abstraction (messages, tooling, providers).
- `server/kaos/` — OS abstraction (paths, stats, filesystem).
- `client/wire-client/` — shared frontend kit: wire client, transcript folding, session listing, remote/tunnel handling.
- `client/kimi-gui/` — native egui frontend (bin: `kimi-gui`).
- `client/kimi-tui/` — ratatui terminal frontend (bin: `kimi-tui`).
- `remote/kimi-bridge/` — relay daemons for remote access (bin: `kimi-bridge`).

## Build & test

```sh
cargo build -p kimi-agent -p kimi-gui -p kimi-tui -p kimi-bridge
cargo test                  # whole workspace
cargo fmt
cargo clippy --workspace --all-targets
```

Run the TUI from a real terminal — it takes over the screen and restores it on
exit:

```sh
./target/debug/kimi-tui -w /some/dir
# Enter sends · Esc cancels the turn · 1/2/3 answer approvals · Tab cycles fork views
# PgUp/PgDn or mouse wheel scrolls · Ctrl+O resume menu · Ctrl+C quits
```

## License & attribution

Apache License 2.0 — see [LICENSE](LICENSE) and [NOTICE](NOTICE). Began as
modified forks of Moonshot AI projects (kimi-agent-rs, and kimi-cli until the
Python side was archived out); per-subtree git history is preserved. An
independent fork, not affiliated with or endorsed by Moonshot AI;
"Kimi"/"Moonshot" are trademarks of their owners.
