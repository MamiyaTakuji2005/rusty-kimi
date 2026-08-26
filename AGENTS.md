# AGENTS.md

Guidance for AI coding agents working in this repository. Read this before making
changes; where these notes and the code disagree, trust the code.

## Project overview

**rusty-kimi** is a Rust-only coding agent system: a wire-only **agent core**
(`kimi-agent`) driven by frontends over the **Wire protocol** (JSON-RPC over
stdio): a native **egui GUI** (`kimi-gui`) and a **terminal UI**
(`kimi-tui`, ratatui).

| Path | What it is | Origin |
|------|------------|--------|
| `server/` | Rust workspace, server side: agent core + LLM/OS abstractions | fork of MoonshotAI/kimi-agent-rs |
| `client/` | Rust workspace, frontends: native GUI, terminal UI, shared frontend kit | fork-authored |
| `_history/` | Historical rewrite PROMPT.md / PLAN.md and dead dev artifacts | context only |

```
kimi-gui (client/, Rust) ──Wire JSON-RPC / stdio──▶ kimi-agent (server/, Rust)
kimi-tui (client/, Rust) ──Wire JSON-RPC / stdio──▶ kimi-agent (server/, Rust)
                       └─(optional)─ kimi-bridge ──TCP (ssh -L)──▶ kimi-agent on a remote box
```

### History, for context only

The fork set out to reconcile an older Rust core with a newer Python shell UI
(vendored fork of MoonshotAI/kimi-cli under `cli/`). All functionality was ported
to Rust; the Python TUI was then reduced to a pure frontend, and once the egui
GUI made it redundant it was **archived to a separate private repo
(`rusty-kimi-tui`) and removed from this tree**. Old commits still contain `cli/`
and the old `core/` layout — treat them as read-only history; do not resurrect or
re-vendor Python code, and do not reintroduce a top-level `core/` directory.

The workspace was later reorganized from a single `core/` directory into
`server/` + `client/` + `remote/` (the relay daemons live in `remote/kimi-bridge`).
The dependency graph deliberately keeps the client depending on the server crate
(`kimi_agent::wire` types, `Session::list`); extracting those into a shared
`wire-protocol` crate is a deferred refactor.

## Repository layout

```
Cargo.toml               workspace root (run cargo here)
server/                  agent server + its abstractions
  kimi-agent/            main crate — wire-only agent server (bin: kimi-agent)
  kosong/                LLM abstraction (messages, tooling, chat providers)
  kaos/                  OS abstraction (LocalKaos, path semantics)
client/                  frontends + shared frontend kit
  kimi-gui/              egui frontend (bin: kimi-gui)
  kimi-tui/              ratatui terminal frontend (bin: kimi-tui)
  wire-client/           shared frontend kit: client + transcript + session list
remote/                  relay daemons for remote access
  kimi-bridge/           byte-relay daemon pair (bin: kimi-bridge; design in remote/PLAN.md)
_history/                historical rewrite PROMPT.md / PLAN.md (Chinese; context only)
```

Per-module notes live in sub-tree `AGENTS.md` files — read them before touching
those areas, and under `server/kimi-agent/src/`: `cli/`, `soul/`, `wire/`,
`tools/`, `skill/`; plus `server/kosong/src/AGENTS.md`, `server/kaos/src/AGENTS.md`.

## Architecture

Two Rust processes. The **frontend never executes LLM steps**; the **agent owns
the loop**.

| Concern | kimi-gui (`client/kimi-gui/`) | kimi-agent (`server/kimi-agent/`) |
|---|---|---|
| GUI, tabs, themes, palette, shortcuts | ✓ | |
| Approval prompts (render + reply) | ✓ | |
| LLM API calls, agent loop/step | | ✓ |
| Context compaction | | ✓ |
| Tool dispatch + built-in tools | | ✓ |
| Skills/flows, prompts, agent specs | | ✓ |
| Session persistence (`~/.kimi`) | | ✓ |
| Wire protocol framing | both | both |
| MCP tool calls | | ✓ (rmcp client) |

- **GUI runtime path**: `kimi-gui` spawns one `kimi-agent` per session tab
  (`session.rs`), speaking JSON-RPC over the child's stdio; the transcript is a
  block stream rendered by `render.rs`; forks appear as sub-tabs.
- **Agent runtime path**: `cli/` (arg parsing; subcommands `info`, `mcp`) →
  `app.rs` (`KimiCLI::create` wiring) → `soul/kimisoul.rs` (loop, steps,
  flow/Ralph runner) → `tools/` dispatch; `wire/server.rs` exposes the stdio
  JSON-RPC server. The agent process owns session persistence (writes its own
  `wire.jsonl` / `context.jsonl` under `~/.kimi`).
- **Wire surface**: the frontend sends `initialize` / `prompt` / `cancel` /
  `replay` / `steer` plus approval replies; the agent answers with typed
  events (`TurnBegin`, `StepBegin`, `ContentPart`, `ToolResult`,
  `StatusUpdate`, `TurnEnd`, …). The full lists are `wire/types.rs` and the
  method dispatch in `wire/server.rs`.

## Compatibility contract (must-follow)

This repo defines its own invariants. They exist to keep existing `~/.kimi` data
readable and any wire-protocol client — including the archived TUI — working
against the agent:

- **Wire protocol**: envelopes, `type` strings, error codes stay stable.
  Version negotiation via `initialize`; current protocol version constant lives
  in `wire/` (see `server/kimi-agent/src/wire/AGENTS.md`).
- **`~/.kimi` data layout** stays identical: `config.toml`, `kimi.json`,
  `mcp.json`, session dirs with `context.jsonl` + `wire.jsonl`.
- **`kosong.message` serde** (e.g. single-`TextPart` `Message.content` → JSON
  string, otherwise an array of parts) stays stable.
- **Tool identifiers** remain `kimi_cli.tools.*`; **wire identity** stays
  "Kimi Code CLI" / `KimiCLI/<VERSION>` even in the Rust binary. These are
  historical IDs, not Python parity — they must simply never change casually.
- Tool schemas, descriptions (`server/kimi-agent/src/tools/desc/`), approvals,
  prompts, and compaction behavior are this repo's own canonical definition now.

Breaking any of these requires a deliberate wire-protocol version bump, not a
casual edit. **Versioning**: the Rust workspace owns its version
(`Cargo.toml`); it is no longer tied to any Python release numbering.

## Build and test commands

Single toolchain — **run cargo from the repo root** (the workspace root is the
top-level `Cargo.toml`). Dev machine is Windows; commands assume Git Bash.

```sh
cargo build -p kimi-agent          # agent binary → target/{debug,release}/kimi-agent
cargo build -p kimi-gui            # native GUI frontend
cargo build -p kimi-tui            # terminal UI frontend
cargo test                         # whole workspace
cargo test -p kimi-agent <name>    # single test
cargo fmt                          # formatting is enforced (see git history)
cargo clippy --workspace --all-targets
```

Workspace rules: edition **2024**, `unsafe_code = "deny"`, `clippy::all = "warn"`.
Prefer async I/O (tokio); avoid blocking locks in async contexts.

### Running the app

```sh
# native GUI:
cargo build -p kimi-gui && ./target/debug/kimi-gui --agent-bin ./target/debug/kimi-agent
# (or set KIMI_AGENT_BIN; remaining args are forwarded to the agent verbatim)

# terminal UI — run inside a real terminal, one session per invocation:
cargo build -p kimi-tui && ./target/debug/kimi-tui -w /some/dir
# (agent binary resolved the same way: --agent-bin flag, KIMI_AGENT_BIN,
#  sibling executable, PATH)

# headless agent with no UI:
./target/debug/kimi-agent

# remote: agent on another box (loopback + ssh tunnel, never raw internet):
#   (VPS)      ./kimi-bridge remote --listen 127.0.0.1:9000
#   (local)    ssh -N -L 9000:127.0.0.1:9000 user@vps
#   (local)    ./kimi-tui --remote 127.0.0.1:9000 -w /path/on/vps
# (agent args resolve on the remote machine; the resume menu lists the
#  remote ~/.kimi; kimi-bridge local --upstream … is an optional extra hop)
```

## Module map

**`server/kimi-agent/src/`** — `cli/` (arg parsing; `info`, `mcp` subcommands),
`app.rs` (`KimiCLI::create` wiring), `soul/` (`kimisoul.rs` loop; `context.rs`
JSONL history with checkpoints/rotations; `approval.rs` queue + YOLO;
`compaction.rs`; `toolset.rs` dispatch + MCP bridge), `wire/` (`types.rs`,
`serde.rs`, `file.rs` JSONL persistence, `server.rs` stdio server, `channel.rs`
merge logic), `tools/` (Shell; `file/` Read/Write/Replace/Glob/Grep/ReadMedia;
`web/` SearchWeb/FetchURL; todo; `agent.rs` Agent + `fork.rs` Fork; `task/`
background task tools; `snapshot.rs` Undo; dmail; think; `test.rs`
plus/compare/panic), `skill/` (skill discovery, mermaid/d2 flows),
`config.rs`/`metadata.rs`/`session.rs`/`share.rs` (persistence), `mcp.rs` (rmcp
client), `prompts/`, `skills/`, `agents/`.

**`server/kosong/src/`** — `message.rs` (canonical message types), `chat_provider/`
(Kimi, Echo, ScriptedEcho — the latter two for tests), `tooling/`, `generate.rs`
(streaming merge + tool-call orchestration).

**`server/kaos/src/`** — `Kaos` trait + `LocalKaos`, task-local `current` override,
`KaosPath` (canonical/expanduser, no symlink resolution), `cached.rs` (glob cache
+ bounded write-undo history), Python-`os.stat`-shaped results (naming is
historical).

**`client/wire-client/src/`** — the shared frontend kit every frontend builds on:
- `lib.rs` — the wire-protocol client used to spawn and drive a `kimi-agent`
  subprocess: JSON-RPC framing, the `Inbound` classification (event /
  reverse-request / response / exit / protocol error), stderr tail capture,
  request-id generation, graceful shutdown. UI-toolkit-free: callers pass a
  `wake` hook (egui: `request_repaint`; the TUI: a channel send) and choose
  console inheritance (`spawn`) or `CREATE_NO_WINDOW`
  (`spawn_without_console`).
- `launch.rs` — agent-binary and remote-endpoint resolution shared by both
  mains (`--agent-bin` → `KIMI_AGENT_BIN` → sibling executable → `PATH`;
  `--remote` → `KIMI_REMOTE`). Deliberately pure: what `--remote` *names* is
  resolved by `remotes.rs`, so this module reads no files.
- `remotes.rs` — the `[[remotes]]` half of `~/.kimi/bridge.toml` (name,
  endpoint, optional tunnel command, default flag) and the name-or-host:port
  resolution behind `--remote`. The daemon reads the `[serve]` half from the
  same file through its own `kimi_bridge::config`; the sections are disjoint
  so neither crate depends on the other.
- `tunnel.rs` — the ssh process a remote is reached through, as a managed
  child (spawn, liveness, stderr tail, kill). Not a shell: the command is
  split, not interpreted, and gets no console — tunnel commands must be
  non-interactive.
- `bridge.rs` — client side of the `kimi-bridge` control framing (the
  daemon-side twin is `remote/kimi-bridge/src/proto.rs`; a drift-guard test
  pins them byte-for-byte), plus the bounded dialling every frontend does
  over it: connect/handshake timeouts, the 64 KiB frame cap, and
  `exit_trailer` — the daemon's final frame, which `start_io` turns into
  `Inbound::AgentExited` so a remote agent's death reads like a local one.
  Both frontends call `connect_tcp` on the thread that draws their UI, so
  nothing in the handshake may block unbounded.
- `transcript.rs` — folds wire events into renderable blocks (moved here from
  kimi-gui so any frontend gets identical state).
- `session_list.rs` — background listing of resumable sessions under `~/.kimi`,
  locally or through a bridge daemon (`spawn_remote_session_listing`).

**`client/kimi-tui/src/`** — the ratatui terminal frontend. One conversation per
invocation (a TUI owns the whole terminal). `main.rs` (event loop over one mpsc
channel fed by crossterm input and the client's wake hook; overlays for
approvals and resume; status bar), `agent.rs` (protocol state machine:
initialize → replay → ready/running/failed, request-id correlation, approval
answering), `input.rs` (single-line editor, char-boundary safe),
`render.rs` (blocks → pre-wrapped styled rows with width-aware wrapping;
scrollback is index arithmetic on the row list).

**`client/kimi-gui/src/`** — `app.rs` (top-level wiring, shortcuts, overlays,
`focus_owner()`), `session.rs` (one agent child + transcript + approval UI per
tab), `render.rs` (transcript block widgets), `theme.rs` (light/dark/Kimi
palettes, moon spinner, `BarStyle`), `palette.rs` (command palette), `os.rs`
(open-in-default-app); transcript/session-list moved to `wire-client`.

**Palette vs. slash commands — a deliberate boundary, held strictly.** The
command palette is for **GUI and orchestration only**: commands that act on the
app and its tabs (open/close/resume sessions, connect a remote, cycle the
theme, open config files and folders). Anything that changes or affects what
a **session** does — compaction, model switching, YOLO, forking, skills and
flows — is a **slash command** owned by the agent (`soul/kimisoul.rs`) and
typed into the session's input, so it behaves identically in every frontend,
including headless wire clients. Do not move session behavior into the palette
(or vice versa): one place per action is what keeps the two from becoming
confusingly overlapping menus.

**`remote/kimi-bridge/`** — the relay daemon pair (`remote/AGENTS.md` has the
map; `remote/PLAN.md` the design record). One binary, two subcommands:
`remote` (agent machine: spawns `kimi-agent` per connection, relays bytes,
answers `list_sessions` from its own `~/.kimi`) and `local` (frontend
machine: forwards frames/bytes upstream). Dumb byte relays — the only thing
either parses is the `BRIDGE1` header line; wire protocol stays untouched.
`WireClient::connect_tcp` is the client-side entry; frontends reach it via
`--remote <host:port>` / `KIMI_REMOTE`. The daemon supplies a default work
directory (its user's home) for sessions that name no `-w`, which is why
kimi-gui's `+` button opens no folder picker in remote mode: this machine's
paths do not exist over there.

## CLI behavior (kimi-agent)

- Wire-only server; no UI selection flags.
- `--wire` exists but is hidden and ignored (legacy compatibility).
- No `--prompt`/`--command` because the wire server does not accept an initial prompt.
- Subcommands: `info`, `mcp` only.
- Help text mirrors the original Python CLI; some MCP examples still show `kimi`.
- Options kept from the original: `--work-dir`, `--session`, `--continue`,
  `--config`, `--config-file`, `--model`, `--thinking/--no-thinking`, `--yolo`,
  `--agent`, `--agent-file`, `--mcp-config-file`, `--mcp-config`, `--skills-dir`,
  `--max-steps-per-turn`, `--max-retries-per-step`, `--max-ralph-iterations`.
- `help_expected` is enabled in clap, so every CLI arg must define help text.

## Code style

- **Rust**: rustfmt clean; community naming/concurrency/error-handling conventions
  (anyhow/thiserror); write detailed comments for public APIs and tricky
  implementations; sub-directory `AGENTS.md` notes for key modules. No `unsafe`.
- **Comments/docs language**: English (the historical rewrite plan in `_history/` is
  Chinese).

## Testing strategy

- Rust tests live in `server/{kimi-agent,kosong,kaos}` and
  `client/{kimi-gui,kimi-tui,wire-client}/` (integration dirs and inline
  `#[cfg(test)]` units); E2E wire tests use ScriptedEcho and mock HTTP
  (`wiremock`, `axum`).
- Wire/data compatibility is verified read-only against real `~/.kimi` data in
  tests — never mutate user session dirs in tests or tooling.

## Build & release

`cargo build -p kimi-agent` / `-p kimi-gui` / `-p kimi-tui` produce
self-contained binaries (macOS/Linux/Windows). CI lives in `.github/workflows/`
(workspace-wide checks; release job packages the `kimi-agent` binary).

## Security considerations

- Approval gating: tools (Shell, WriteFile, StrReplaceFile, …) require user
  approval; YOLO mode auto-approves — be deliberate when enabling it.
- Shell tool on Windows prefers **Git Bash**; per-call timeout is floored at 30 s.
- MCP: config in `~/.kimi/mcp.json`; Rust uses `rmcp`. MCP clients must not
  auto-inject `mcp-session-id` headers (some standard servers reject them).
- `.claude/settings.local.json` contains local, machine-specific permission
  grants — it is not project configuration; do not treat its paths as portable.

## Misc conventions

- Git: single `main` branch; subtree history preserved. Do not run
  `git commit/push/reset/rebase` unless explicitly asked.
- The repo lives at `C:\Users\MamiyaTakuji\.rusty-kimi\rusty-kimi` on this
  machine; the shell tool may start in a different cwd — always `cd` into the
  repo root explicitly.
- When changing wire-visible behavior, update the tests and the relevant
  sub-tree `AGENTS.md` in the same change. If the change breaks wire/data
  compatibility, bump the protocol version deliberately.
- The client intentionally depends on the server crate (`kimi_agent::wire`
  types, `Session::list`). Extracting those into a shared crate is a deferred
  refactor; do not start it as a side quest.
