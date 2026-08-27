# DvaDva

A self-contained coding agent: a **Rust agent core** driven by native Rust
frontends over the **Wire protocol** (JSON-RPC over stdio). Everything in the
runtime path is Rust.

```
inkvizitor (Rust, egui)    ──Wire JSON-RPC / stdio──▶  dvadva-agent (Rust)
dvadva-tui (Rust, ratatui) ──Wire JSON-RPC / stdio──▶  dvadva-agent (Rust)
                  └─(optional)─ dvadva-bridge ──TCP (ssh -L)──▶ dvadva-agent on a remote box
```

An agent can run detached: frontends attach, leave, and rejoin — see
[Detached agents](#detached-agents).

## The pieces

**`dvadva-agent`** (`server/dvadva-agent/`) is the agent server — wire-only, no UI
of its own. It owns the agent loop, LLM provider calls (Kimi,
OpenAI-compatible, Echo variants for tests), context management and
auto-compaction, the built-in tools (Shell, file Read/Write/Replace/Glob/Grep,
web search/fetch, todo, subagents/fork, undo, dmail, think), skills and flows,
MCP, and session persistence under `~/.kimi`. With `--listen` it serves
several clients over loopback TCP and outlives them, registered under
`~/.kimi/live/`.

**`inkvizitor`** (`client/inkvizitor/`) is the native egui frontend and canonical
client: sessions in tabs (a local `dvadva-agent` subprocess each, or an
attached remote one; forks as sub-tabs), splittable into panes that each view
the same set of tabs, full keyboard control, a `Ctrl+P` command palette,
light/dark/Kimi themes, approval prompts, a live status bar, and the streamed
transcript.

**`dvadva-tui`** (`client/dvadva-tui/`) is the ratatui terminal frontend — one
session per invocation; a remote session is attached, not owned. (The original
Python TUI is archived in a separate private repo.)

**The Wire protocol is the contract.** Frontends send `initialize`, `prompt`,
`cancel`, `replay`, `steer`, and `shutdown` plus approval replies; the agent
streams back typed events (`TurnBegin`, `ContentPart`, `ToolResult`,
`TurnEnd`, …). The protocol, the `~/.kimi` data layout, and the
`kimi_cli.tools.*` tool identity are compatibility invariants: any client
speaking the protocol can drive the agent.

## Quick start

```sh
cargo build -p dvadva-agent -p inkvizitor
./target/debug/inkvizitor --agent-bin ./target/debug/dvadva-agent
```

- `--agent-bin <path>` (or `KIMI_AGENT_BIN`) points a frontend at the agent
  binary; remaining arguments are forwarded to the agent verbatim (`-w <dir>`,
  `--session <id>`, `--continue`, `--model <name>`, …).
- Run `dvadva-agent` on its own for a headless server; `--listen [addr]`
  serves it over loopback TCP, detached.

> **Windows note:** "Access is denied" replacing `dvadva-agent.exe` during a
> build means a running `inkvizitor`/`dvadva-agent` holds the binary — close it
> and rebuild.

## Remote access: `dvadva-bridge`

Run the agent on another machine and drive it from a local frontend. The
bridge is a pair of **dumb byte relays** — they never parse the wire protocol.
No auth, no TLS: keep both ends on loopback and cross the network through an
ssh tunnel.

```sh
# on the remote box:
./dvadva-bridge remote --listen 127.0.0.1:9000

# on the local box (kept open, or configured as a `tunnel` below):
ssh -N -L 9000:127.0.0.1:9000 user@remote

# from any local terminal (no -w needed: the daemon supplies its own):
./dvadva-tui --remote 127.0.0.1:9000
```

**One config file, both roles.** `~/.kimi/bridge.toml` describes how a machine
serves (`[serve]`) and which remotes it can reach (`[[remotes]]`); each half is
read only by the side that needs it. It is separate from `config.toml` because
the agent rewrites that file and would drop unknown sections.

```toml
# On the VPS: `dvadva-bridge remote` with no arguments then does the right thing.
[serve]
listen = "127.0.0.1:9000"
work_dir = "/home/kimi"       # default for sessions that pass no -w
agent_idle_timeout = 3600     # stop agents nobody rejoins (0 disables)

# On your machine: the remotes the frontends can reach.
[[remotes]]
name = "vps"
endpoint = "127.0.0.1:9000"                            # this end of the tunnel
tunnel = "ssh -N -L 9000:127.0.0.1:9000 user@vps"      # optional, run/killed by the GUI
default = true
```

`--remote` takes a **name or a `host:port`** (`inkvizitor --remote vps`, or
`--remote 127.0.0.1:9000` with no config at all) and says where the first
session opens. Flags beat the config file, which beats built-in defaults.

**Chain buttons.** inkvizitor shows one chain button per `[[remotes]]` entry,
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
sessions of whichever machine you are looking at, marking the ones an agent
is hosting — picking one joins that agent (see
[Detached agents](#detached-agents)). A session that names no `-w`
gets the daemon's default work directory (`[serve] work_dir`, else the remote
user's home) — a frontend on another OS cannot name a path that exists over
there, so it doesn't have to. Design record: [`remote/PLAN.md`](remote/PLAN.md).

**Running it as a service.** `dvadva-agent` reads credentials from the
environment (`KIMI_API_KEY`, …) and config/sessions from `~/.kimi`, so the unit
must supply `HOME`:

```ini
[Service]
Environment=HOME=/home/kimi
EnvironmentFile=/home/kimi/.config/dvadva-bridge.env   # KIMI_API_KEY=…
WorkingDirectory=/home/kimi
ExecStart=/usr/local/bin/dvadva-bridge remote      # reads [serve] from bridge.toml
Restart=on-failure
User=kimi
```

Keep the listen address on loopback: anything that reaches that port can run
commands as this user. Use a dedicated unprivileged user and let ssh be the
only way in.

## Detached agents

An agent can outlive its clients. `--listen [addr]` (loopback only) serves the
wire over TCP next to stdio: clients attach and leave, the agent keeps its
turn and its context, and it registers itself under `~/.kimi/live/` so another
process can find it. `--idle-timeout <secs>` ends an agent with nobody
attached and nothing to do for that long — a turn parked on an approval with
no client counts as idle, a working turn never does. Off by default: whoever
typed `--listen` owns the process.

Remote sessions are attached, not owned. Closing the tab or quitting the TUI
leaves the agent running on the far machine; reopening the session rejoins it,
because `--remote` attaches by session id. The resume menus mark live
sessions, and picking one joins the running agent instead of starting another.
A connection that drops is a *detach*, not a failure: inkvizitor retries with
a capped backoff and then offers a button, dvadva-tui binds `Ctrl+R` (and
prints the rejoin command on exit). Rejoining finds the live agent or starts a
fresh one on the same session files — the frontend never knows which happened.

A detached agent ends three ways, all through one stop token: a signal, the
wire's `shutdown` method (inkvizitor's "Stop agent", dvadva-tui's `Ctrl+K` —
quit *and* stop), and the idle timeout. `dvadva-bridge remote` supervises the
agents it starts: they get `--listen` and `--idle-timeout` prepended
(`[serve] agent_idle_timeout`), because the daemon is where agents accumulate.
Local sessions stay stdio children that end when closed — a local `--listen`
agent can be joined from either frontend, but the frontends do not spawn local
sessions that way. Design record:
[PLAN-detached-agent.md](PLAN-detached-agent.md).

## Repo layout

The workspace root is the top-level `Cargo.toml`; run cargo from the repo root.

- `server/dvadva-agent/` — the agent server (bin: `dvadva-agent`), wire-only.
- `server/kosong/` — LLM abstraction (messages, tooling, providers).
- `server/kaos/` — OS abstraction (paths, stats, filesystem).
- `client/wire-client/` — shared frontend kit: wire client, transcript folding, session listing, remote/tunnel handling.
- `client/inkvizitor/` — native egui frontend (bin: `inkvizitor`).
- `client/dvadva-tui/` — ratatui terminal frontend (bin: `dvadva-tui`).
- `remote/dvadva-bridge/` — relay daemons for remote access (bin: `dvadva-bridge`).

## Build & test

```sh
cargo build -p dvadva-agent -p inkvizitor -p dvadva-tui -p dvadva-bridge
cargo test                  # whole workspace
cargo fmt
cargo clippy --workspace --all-targets
```

Run the TUI from a real terminal — it takes over the screen and restores it on
exit:

```sh
./target/debug/dvadva-tui -w /some/dir
# Enter sends · Esc cancels the turn · 1/2/3 answer approvals · Tab cycles fork views
# PgUp/PgDn or mouse wheel scrolls · Ctrl+O resume menu · Ctrl+R rejoins a detach
# Ctrl+C quits · Ctrl+K quits and stops a remote agent
```

## License & attribution

Apache License 2.0 — see [LICENSE](LICENSE) and [NOTICE](NOTICE). Began as
modified forks of Moonshot AI projects ([kimi-agent-rs][upstream], and kimi-cli
until the Python side was archived out); per-subtree git history is preserved.

**On the name.** This started as a fork of kimi-agent-rs and was called
rusty-kimi, but almost nothing of the original runtime survives — the
frontends, the wire protocol, the tool surface, and most of the server are
this project's own work. Apache-2.0 grants the code and explicitly not the
marks, so continuing to ship under Moonshot's name would misattribute this
fork's bugs to them. Hence **DvaDva**: Dostoevsky's twice-two-makes-four, the
arithmetic that stands for a world where everything is already determined —
and, doubled, a fork that came to replace its original. The GUI is called
`inkvizitor`, since it is the part that interrogates you before anything is
allowed to happen. Not affiliated with or endorsed by Moonshot AI;
"Kimi"/"Moonshot" are trademarks of their owners.

[upstream]: https://github.com/MoonshotAI/kimi-agent-rs
