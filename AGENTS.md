# AGENTS.md

Guidance for AI coding agents working in this repository. Read this before making
changes; where these notes and the code disagree, trust the code.

## Project overview

**DvaDva** is a Rust-only coding agent system: a wire-only **agent core**
(`dvadva-agent`) driven by frontends over the **Wire protocol** (JSON-RPC over
stdio): a native **egui GUI** (`inkvizitor`) and a **terminal UI**
(`dvadva-tui`, ratatui).

| Path | What it is | Origin |
|------|------------|--------|
| `server/` | Rust workspace, server side: agent core + LLM/OS abstractions | fork of MoonshotAI/kimi-agent-rs |
| `client/` | Rust workspace, frontends: native GUI, terminal UI, shared frontend kit | fork-authored |
| `_history/` | Historical rewrite PROMPT.md / PLAN.md and dead dev artifacts | context only |

```
inkvizitor (client/, Rust) ──Wire JSON-RPC / stdio──▶ dvadva-agent (server/, Rust)
dvadva-tui (client/, Rust) ──Wire JSON-RPC / stdio──▶ dvadva-agent (server/, Rust)
                       └─(optional)─ dvadva-bridge ──TCP (ssh -L)──▶ dvadva-agent on a remote box
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
`server/` + `client/` + `remote/` (the relay daemons live in `remote/dvadva-bridge`).
The dependency graph deliberately keeps the client depending on the server crate
(`dvadva_agent::wire` types, `Session::list`); extracting those into a shared
`wire-protocol` crate is a deferred refactor.

### The rename (2026-08, in progress)

The project was called **rusty-kimi** until so little of upstream remained that
shipping under Moonshot's marks became misattribution rather than credit —
Apache-2.0 grants the code, never the name. Crates, libraries, and binaries are
now `dvadva-agent`, `dvadva-tui`, `dvadva-bridge`, and `inkvizitor` (the GUI;
its process name is the joke — it is the part that interrogates you before
anything happens).

**What still says "kimi" does so on purpose. Do not sweep it away:**

| Still named kimi | Why |
|---|---|
| `kosong/src/chat_provider/kimi.rs`, `KIMI_API_KEY`, `KIMI_BASE_URL`, `KimiStreamedMessage` | these name **Moonshot's actual API**, exactly like `openai_compatible.rs` does OpenAI's. Renaming them would be wrong, not brave. |
| `kimi_cli.tools.*` tool ids, `"Kimi Code CLI"`, `KimiCLI/<VERSION>` | the wire contract (see the compatibility section). Changing them needs a protocol bump plus an alias table for stored sessions and agent specs. |
| `~/.kimi`, `kimi.json` | user data. Needs a one-time migration that renames the directory only when the new one does not exist yet. |
| other `KIMI_*` environment variables | need a release that reads the old name as a fallback, or every existing service unit breaks silently. |
| `kimi-agent-rs`, `rusty-kimi-tui` | real repository names — upstream, and this project's archived Python TUI. |
| the `Kimi` theme | an homage to the original's palette; cosmetic, rename it or don't. |

Internal type names (`KimiSoul`, `KimiToolset`, `KimiCliError`, the `KimiCLI`
app struct, `soul/kimisoul.rs`) are *not* under any contract — they simply have
not been renamed yet. That is a free, compiler-verified change whenever someone
picks the target names.

## Repository layout

```
Cargo.toml               workspace root (run cargo here)
server/                  agent server + its abstractions
  dvadva-agent/            main crate — wire-only agent server (bin: dvadva-agent)
  kosong/                LLM abstraction (messages, tooling, chat providers)
  kaos/                  OS abstraction (LocalKaos, path semantics)
client/                  frontends + shared frontend kit
  inkvizitor/              egui frontend (bin: inkvizitor)
  dvadva-tui/              ratatui terminal frontend (bin: dvadva-tui)
  dvadva-android/          Kotlin/Compose phone frontend (attaches over WireGuard; not Cargo)
  wire-client/           shared frontend kit: client + transcript + session list
remote/                  relay daemons for remote access
  dvadva-bridge/           byte-relay daemon pair (bin: dvadva-bridge; design in remote/PLAN.md)
_history/                historical rewrite PROMPT.md / PLAN.md (Chinese; context only)
PLAN-detached-agent.md   design record: headless agent + attach/detach (phases 0-1 done, 2 begun)
```

Per-module notes live in sub-tree `AGENTS.md` files — read them before touching
those areas, and under `server/dvadva-agent/src/`: `cli/`, `soul/`, `wire/`,
`tools/`, `skill/`; plus `server/kosong/src/AGENTS.md`, `server/kaos/src/AGENTS.md`.

## Architecture

Two Rust processes. The **frontend never executes LLM steps**; the **agent owns
the loop**.

| Concern | inkvizitor (`client/inkvizitor/`) | dvadva-agent (`server/dvadva-agent/`) |
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

- **GUI runtime path**: `inkvizitor` spawns one `dvadva-agent` per session tab
  (`session.rs`), speaking JSON-RPC over the child's stdio; the transcript is a
  block stream rendered by `render.rs`; forks appear as sub-tabs.
- **Agent runtime path**: `cli/` (arg parsing; subcommands `info`, `mcp`) →
  `app.rs` (`KimiCLI::create` wiring) → `soul/kimisoul.rs` (loop, steps,
  flow/Ralph runner) → `tools/` dispatch; `wire/server.rs` exposes the
  JSON-RPC server. The agent process owns session persistence (writes its own
  `wire.jsonl` / `context.jsonl` under `~/.kimi`).
- **Several clients, one agent**: `serve_connection` serves one attached
  client over any reader/writer pair, and stdio is only its first caller. What
  the session owns (one turn at a time, the open approvals, the toolset) lives
  on `SessionCore`; what a client owns (initialized, catching up, its external
  tools) lives on `Connection`. Events broadcast, responses are unicast; see
  `wire/AGENTS.md` for the rules.
- **Detach without dying**: with `--listen [ADDR]` the agent also serves a
  loopback socket (`wire/listener.rs`), and there a client leaving is a
  detach rather than a kill — the turn keeps running, and the next client to
  attach replays into it. Over plain stdio the pipe is still the lifetime,
  deliberately: that is the one-shot path. The socket binds loopback only and
  takes a token from the session's `attach.token`, checked before any wire
  byte is read. The bound address is announced on stderr as
  `dvadva-agent: listening {json}`, which is how a supervisor learns an
  ephemeral port. The rest of Phase 2 (bridge supervisor, live-session
  registry) is in `PLAN-detached-agent.md`.
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
  `WIRE_PROTOCOL_VERSION` in `wire/protocol.rs` is `major.minor`: **a major
  bump is breaking, a minor bump is additive only** — new message types, new
  optional fields, never a changed meaning. Both ends check it through
  `initialize` and refuse a foreign major (`check_peer`; the agent answers
  `PROTOCOL_VERSION_MISMATCH`, the frontends fail the session with the same
  text). A peer's *higher* minor is safe to talk to — ignore what you do not
  recognize — and a peer's *lower* minor means do not use what it predates,
  which is what `ProtocolVersion::has` is for. 1.3 added the `initialize`
  result's `capabilities` object; ask it, do not infer capability from a
  version number.
- **A client's own JSON-RPC ids are not addresses.** They come from
  `WireClient::next_id` and are unique only within one connection — two
  attached clients both call their first request `"1"`. Nothing on the agent
  side may key state by them or route by them. Ids the *agent* mints for
  reverse-RPC (approvals, external tool calls) are globally unique and are
  the only ones safe to use as keys.
- **Bridge frame protocol**: `remote/dvadva-bridge/src/proto.rs`, versioned on
  its own clock and duplicated byte-for-byte in `wire-client/src/bridge.rs`
  (a dev-dependency test pins the two). The digit in the `BRIDGE1` magic is
  its major and is a hard gate; `BRIDGE_PROTOCOL_VERSION` adds the minor, and
  a `version` reply carries it so an additive op needs no BRIDGE2. The daemons
  never parse the wire, so this version and the wire's move independently.
- **The agent's CLI surface is a contract too.** The frontends *generate*
  agent argv (`inkvizitor/src/app.rs` builds `-w` and `--session`) and ship it
  through the bridge to be executed on the far machine. Renaming a flag in
  `cli/mod.rs` breaks remote resume with the wire protocol untouched, so
  frontend-generated flags are **append-only**.

**Version numbers say two different things and must not be merged.** A
component version (every crate is `version.workspace = true`) says *which
build* you are talking to; a protocol version says *whether you can talk to it
at all*. Only the second decides compatibility, and each protocol owns its
own. Report both wherever a running binary identifies itself —
`dvadva-agent --version`, `dvadva-agent info`, the bridge's startup banner,
the bridge `version` probe behind the GUI's connection light.
- **`~/.kimi` data layout** stays identical: `config.toml`, `kimi.json`,
  `mcp.json`, session dirs with `context.jsonl` + `wire.jsonl`.
- **`kosong.message` serde** (e.g. single-`TextPart` `Message.content` → JSON
  string, otherwise an array of parts) stays stable.
- **Tool identifiers** remain `kimi_cli.tools.*`; **wire identity** stays
  "Kimi Code CLI" / `KimiCLI/<VERSION>` even in the Rust binary. These are
  historical IDs, not Python parity — they must simply never change casually.
  The 2026-08 rename to DvaDva deliberately stopped short of both: they are a
  contract, and re-cutting them is a deliberate versioned change (with an alias
  table for ids already written into sessions and agent specs), not a sweep.
- Tool schemas, descriptions (`server/dvadva-agent/src/tools/desc/`), approvals,
  prompts, and compaction behavior are this repo's own canonical definition now.

Breaking any of these requires a deliberate wire-protocol version bump, not a
casual edit. **Versioning**: the Rust workspace owns its version
(`Cargo.toml`); it is no longer tied to any Python release numbering.

## Build and test commands

Single toolchain — **run cargo from the repo root** (the workspace root is the
top-level `Cargo.toml`). Dev machine is Windows; commands assume Git Bash.

```sh
cargo build -p dvadva-agent          # agent binary → target/{debug,release}/dvadva-agent
cargo build -p inkvizitor            # native GUI frontend
cargo build -p dvadva-tui            # terminal UI frontend
cargo test                         # whole workspace
cargo test -p dvadva-agent <name>    # single test
cargo fmt                          # formatting is enforced (see git history)
cargo clippy --workspace --all-targets
```

Workspace rules: edition **2024**, `unsafe_code = "deny"`, `clippy::all = "warn"`.
Prefer async I/O (tokio); avoid blocking locks in async contexts.

### Running the app

```sh
# native GUI:
cargo build -p inkvizitor && ./target/debug/inkvizitor --agent-bin ./target/debug/dvadva-agent
# (or set KIMI_AGENT_BIN; remaining args are forwarded to the agent verbatim)

# terminal UI — run inside a real terminal, one session per invocation:
cargo build -p dvadva-tui && ./target/debug/dvadva-tui -w /some/dir
# (agent binary resolved the same way: --agent-bin flag, KIMI_AGENT_BIN,
#  sibling executable, PATH)

# headless agent with no UI:
./target/debug/dvadva-agent

# remote: agent on another box (loopback + ssh tunnel, never raw internet):
#   (VPS)      ./dvadva-bridge remote --listen 127.0.0.1:9000
#   (local)    ssh -N -L 9000:127.0.0.1:9000 user@vps
#   (local)    ./dvadva-tui --remote 127.0.0.1:9000 -w /path/on/vps
# (agent args resolve on the remote machine; the resume menu lists the
#  remote ~/.kimi; dvadva-bridge local --upstream … is an optional extra hop)
```

## Module map

**`server/dvadva-agent/src/`** — `cli/` (arg parsing; `info`, `mcp` subcommands),
`app.rs` (`KimiCLI::create` wiring), `soul/` (`kimisoul.rs` loop; `context.rs`
JSONL history with checkpoints/rotations; `approval.rs` queue + YOLO;
`compaction.rs`; `toolset.rs` dispatch + MCP bridge), `wire/` (`types.rs`,
`serde.rs`, `file.rs` JSONL persistence, `server.rs` stdio server, `channel.rs`
merge logic), `tools/` (Shell; `file/` Read/Write/Replace/Glob/Grep/ReadMedia;
`web/` SearchWeb/FetchURL; todo; `agent.rs` Agent + `fork.rs` Fork; `task/`
background task tools; `snapshot.rs` Undo; dmail; think; `test.rs`
plus/compare/panic), `skill/` (skill discovery, mermaid/d2 flows),
`config.rs`/`metadata.rs`/`session.rs`/`share.rs` (persistence; metadata saves
go through a temp file + rename and log failures instead of panicking), `mcp.rs`
(rmcp client), `prompts/`, `skills/`, `agents/`.

**`server/kosong/src/`** — `message.rs` (canonical message types), `chat_provider/`
(Kimi, Echo, ScriptedEcho — the latter two for tests), `tooling/`, `generate.rs`
(streaming merge + tool-call orchestration). The provider HTTP clients are
deliberately bounded — 10 s connect, 120 s between stream chunks — so a stalled
upstream surfaces as a retryable `Timeout` instead of wedging the step forever;
do not remove those timeouts.

**`server/kaos/src/`** — `Kaos` trait + `LocalKaos`, task-local `current` override,
`KaosPath` (canonical/expanduser, no symlink resolution), `cached.rs` (glob cache
+ bounded write-undo history), Python-`os.stat`-shaped results (naming is
historical).

**`client/wire-client/src/`** — the shared frontend kit every frontend builds on:
- `lib.rs` — the wire-protocol client used to spawn and drive a `dvadva-agent`
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
  same file through its own `dvadva_bridge::config`; the sections are disjoint
  so neither crate depends on the other.
- `tunnel.rs` — the ssh process a remote is reached through, as a managed
  child (spawn, liveness, stderr tail, kill). Not a shell: the command is
  split, not interpreted, and gets no console — tunnel commands must be
  non-interactive.
- `bridge.rs` — client side of the `dvadva-bridge` control framing (the
  daemon-side twin is `remote/dvadva-bridge/src/proto.rs`; a drift-guard test
  pins them byte-for-byte), plus the bounded dialling every frontend does
  over it: connect/handshake timeouts, the 64 KiB frame cap, and
  `exit_trailer` — the daemon's final frame, which `start_io` turns into
  `Inbound::AgentExited` so a remote agent's death reads like a local one.
  Both frontends call `connect_tcp` on the thread that draws their UI, so
  nothing in the handshake may block unbounded.
- `transcript.rs` — folds wire events into renderable blocks (moved here from
  inkvizitor so any frontend gets identical state).
- `session_list.rs` — background listing of resumable sessions under `~/.kimi`,
  locally or through a bridge daemon (`spawn_remote_session_listing`).

**`client/dvadva-tui/src/`** — the ratatui terminal frontend. One conversation per
invocation (a TUI owns the whole terminal). `main.rs` (event loop over one mpsc
channel fed by crossterm input and the client's wake hook; overlays for
approvals and resume; status bar; a panic hook restores the terminal before
the panic message prints), `agent.rs` (protocol state machine:
initialize → replay → ready/running/failed, request-id correlation, approval
answering), `input.rs` (single-line editor, char-boundary safe; the cursor
column is display width, so CJK counts two cells),
`render.rs` (blocks → pre-wrapped styled rows with width-aware wrapping;
scrollback is index arithmetic on the row list).

**`client/dvadva-android/`** — the Android phone frontend (Kotlin + Compose;
one conversation per run, like the TUI). A deliberate **port, not a fork**:
`app/.../proto/` mirrors `wire-client`'s `bridge.rs`/`lib.rs` and the agent's
wire types in pure Kotlin (no Android imports, JVM-unit-testable), pinned to
the Rust side by golden-vector tests asserting the same frames the Rust
suites assert; `session/Transcript.kt` is a simplified fold of
`transcript.rs`. It is a dumb TCP client over the phone's *existing*
WireGuard tunnel to a `dvadva-bridge` daemon — no embedded tunnel, no
VpnService, `INTERNET` as its only permission. Not part of the Cargo
workspace; its own `AGENTS.md` has the rules and build notes (the Gradle
wrapper jar is generated, not committed).

**`client/inkvizitor/src/`** — `app.rs` (top-level wiring, shortcuts, overlays,
`focus_owner()`, the split panes; holds one `RemoteLink` per configured
`[[remotes]]` entry — the tab strip paints a chain button each, and
`pick_link` resolves which one a remote command means), `session.rs` (one
agent child + transcript + approval UI per tab), `render.rs` (transcript
block widgets), `remote_link.rs`
(per-remote connection state machine: tunnel child, background version probe,
the painted chain button), `theme.rs` (light/dark/Kimi palettes, moon spinner,
`BarStyle`), `palette.rs` (command palette), `os.rs` (open-in-default-app);
transcript/session-list moved to `wire-client`.

**The mark** lives in the workspace's `assets/`, not in any one crate:
`make_icon.py` draws **2 squared** -- the arithmetic the project is named for
-- as a cyan tile at seven sizes into `dvadva.ico`, and all three Windows
binaries embed that one file through a near-identical `build.rs`
(`winresource`, host-gated in each `Cargo.toml` so the Linux build never
carries it). That resource *is* the .exe's own icon in Explorer, which no
run-time call can set; inkvizitor additionally hands eframe `icon-64.rgba`
for the **window** icon, raw RGBA so nothing has to pull the `image` crate in
to decode 16 KB, with a compile-time assert in `main.rs` catching a
regenerated file of another size. A failed `res.compile()` warns and carries
on: the icon is cosmetic, and a machine without the SDK's `rc.exe` should
still get a working binary.

Three traps are commented where they bite. Pillow silently **drops any .ico
size larger than the image `save` was called on**, so the call must be made
on the largest tile or the file quietly comes out 16px and nothing else; the
small sizes are drawn at 8x and filtered down, since a 16px tile straight out
of a hinted font loses the exponent; and the script writes beside itself
rather than into the working directory, so running it from the repo root does
not scatter three files across it.

**Split panes are duplicate views, not workspaces.** The window divides into
`app.rs`'s `panes: Vec<Pane>` along one `Split` axis for the whole window —
columns or rows, no tree. Every pane draws the *whole* scene: its own tab
strip listing **every** session, its own active tab, its own scroll position
and chat box. Only the choice of tab is per-pane; the sessions, the remote
links and the overlays are the window's. The overlays deliberately stay
single — palette, resume list, close confirmation and errors are centered
windows, and two copies would fight over the same middle of the screen — so
they act on `self.focused`, which `Alt+arrow` moves along the split axis and a
click into a pane also moves. `Alt`, not `Shift`: shortcuts are consumed
before any widget sees them and `Shift+arrow` is how text is selected in the
permanently focused chat box.

Three mechanical consequences, all easy to reintroduce by accident:

- **A pane's panel id must name the axis** (`pane_id`). egui keeps one
  `PanelState` per id and reads a *width* out of it for a `SidePanel` but a
  *height* for a `TopBottomPanel`, so an id shared between the two hands a row
  split the full-window height its column incarnation stored: the top pane
  claims everything, the one below gets nothing, and the split looks like a
  no-op — then does it in mirror image the next time you split the other way.
  The pane count is in the id too, so each layout remembers its own dividers.
- **Ids are salted by pane.** `Session::ui` takes a `PaneSlot`, and every id
  under it (`push_id`, the chat box, the approval window) carries
  `slot.index`. Without it, the same session open in two panes is one widget
  drawn twice: shared scroll offset, shared focus, egui id collisions.
- **Only the focused pane writes frame state.** `input_had_focus` and
  `input_at_start` live on the `Session`, are read next frame by keys consumed
  before the widget redraws, and are guarded by `slot.focused` — otherwise the
  idle copy, drawn afterwards, reports its own unfocused box and Enter stops
  sending in the pane being typed in. Same guard on the approval
  focus-surrender. `slot.columns` is the other half: the transcript's wrap
  floor is a fraction of the *monitor*, so it has to be divided among panes
  sharing the width or both halves of a split clip at every size.

**Palette vs. slash commands — a deliberate boundary, held strictly.** The
command palette is for **GUI and orchestration only**: commands that act on the
app, its tabs and its panes (open/close/resume sessions, connect a remote,
split and unsplit, cycle the theme, open config files and folders). Anything that changes or affects what
a **session** does — compaction, model switching, YOLO, forking, skills and
flows — is a **slash command** owned by the agent (`soul/kimisoul.rs`) and
typed into the session's input, so it behaves identically in every frontend,
including headless wire clients. Do not move session behavior into the palette
(or vice versa): one place per action is what keeps the two from becoming
confusingly overlapping menus.

The remote palette commands take an optional **trailing remote name**
(`open remote session vps`); the bare command means the default remote (the
entry marked `default = true` in `bridge.toml`, else the first) — the same
contract as the CLI's `--remote`. The query split lives in `palette.rs`
(`Match::arg`, only for `takes_remote` entries); name resolution and the
typo error happen at run time in `app.rs` (`pick_link`/`target_link`), so
mid-typing never blanks the palette list.

**`remote/dvadva-bridge/`** — the relay daemon pair (`remote/AGENTS.md` has the
map; `remote/PLAN.md` the design record). One binary, two subcommands:
`remote` (agent machine: spawns `dvadva-agent` per connection, relays bytes,
answers `list_sessions` from its own `~/.kimi`) and `local` (frontend
machine: forwards frames/bytes upstream). Dumb byte relays — the only thing
either parses is the `BRIDGE1` header line; wire protocol stays untouched.
`WireClient::connect_tcp` is the client-side entry; frontends reach it via
`--remote <host:port>` / `KIMI_REMOTE`. The daemon supplies a default work
directory (its user's home) for sessions that name no `-w`, which is why
inkvizitor's `+` button opens no folder picker in remote mode: this machine's
paths do not exist over there.

## CLI behavior (dvadva-agent)

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

- Rust tests live in `server/{dvadva-agent,kosong,kaos}` and
  `client/{inkvizitor,dvadva-tui,wire-client}/` (integration dirs and inline
  `#[cfg(test)]` units); E2E wire tests use ScriptedEcho and mock HTTP
  (`wiremock`, `axum`).
- Wire/data compatibility is verified read-only against real `~/.kimi` data in
  tests — never mutate user session dirs in tests or tooling.

## Build & release

`cargo build -p dvadva-agent` / `-p inkvizitor` / `-p dvadva-tui` produce
self-contained binaries (macOS/Linux/Windows). CI lives in `.github/workflows/`
(workspace-wide checks; release job packages the `dvadva-agent` binary).

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
- The client intentionally depends on the server crate (`dvadva_agent::wire`
  types, `Session::list`). Extracting those into a shared crate is a deferred
  refactor; do not start it as a side quest.
