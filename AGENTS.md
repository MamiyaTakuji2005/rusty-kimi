# AGENTS.md

Guidance for AI coding agents working in this repository. Read this before making changes.
`CLAUDE.md` (repo root) carries a condensed version of the same rules — when they
conflict with reality, trust the code.

## Project overview

**rusty-kimi** is a Rust-only coding agent system: a wire-only **agent core**
(`kimi-agent`) driven by frontends over the **Wire protocol** (JSON-RPC over
stdio): a native **egui GUI** (`kimi-gui`) and a **terminal UI**
(`kimi-tui`, ratatui).

| Path | What it is | Origin |
|------|------------|--------|
| `core/` | Rust workspace: agent core, LLM/OS abstractions, native GUI, terminal UI | fork of MoonshotAI/kimi-agent-rs + the fork-authored `kimi-gui` / `kimi-tui` |

```
kimi-gui (core/, Rust) ──Wire JSON-RPC / stdio──▶ kimi-agent (core/, Rust)
kimi-tui (core/, Rust) ──Wire JSON-RPC / stdio──▶ kimi-agent (core/, Rust)
```

### History, for context only

The fork set out to reconcile an older Rust core with a newer Python shell UI
(vendored fork of MoonshotAI/kimi-cli under `cli/`). All functionality was ported
to Rust; the Python TUI was then reduced to a pure frontend, and once the egui
GUI made it redundant it was **archived to a separate private repo
(`rusty-kimi-tui`) and removed from this tree**. Old commits still contain `cli/`
— treat it as read-only history; do not resurrect or re-vendor Python code.

## Repository layout

```
core/                  Rust workspace (run cargo here)
  kimi-agent/          main crate — wire-only agent server (bin: kimi-agent)
  kosong/              LLM abstraction (messages, tooling, chat providers)
  kaos/                OS abstraction (LocalKaos, path semantics)
  kimi-gui/            egui frontend (bin: kimi-gui)
  kimi-tui/            ratatui terminal frontend (bin: kimi-tui)
  wire-client/         shared frontend kit: client + transcript + session list
  _/                   historical rewrite PROMPT.md / PLAN.md (Chinese; context only)
```

Per-module notes live in sub-tree `AGENTS.md` files — read them before touching
those areas: `core/AGENTS.md` (workspace overview, compatibility contract), and
under `core/kimi-agent/src/`: `cli/`, `soul/`, `wire/`, `tools/`, `skill/`; plus
`core/kosong/src/AGENTS.md`, `core/kaos/src/AGENTS.md`.

## Architecture

Two Rust processes. The **frontend never executes LLM steps**; the **agent owns
the loop**.

| Concern | kimi-gui (`core/kimi-gui/`) | kimi-agent (`core/kimi-agent/`) |
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

## Compatibility contract (must-follow)

With the Python tree archived out, this repo defines its own invariants. These
exist to keep existing `~/.kimi` data readable and any wire-protocol client —
including the archived TUI — working against the agent:

- **Wire protocol**: envelopes, `type` strings, error codes stay stable.
  Version negotiation via `initialize`; current protocol version constant lives
  in `wire/` (see `core/kimi-agent/src/wire/AGENTS.md`).
- **`~/.kimi` data layout** stays identical: `config.toml`, `kimi.json`,
  `mcp.json`, session dirs with `context.jsonl` + `wire.jsonl`.
- **`kosong.message` serde** (e.g. single-`TextPart` `Message.content` → JSON
  string, otherwise an array of parts) stays stable.
- **Tool identifiers** remain `kimi_cli.tools.*`; **wire identity** stays
  "Kimi Code CLI" / `KimiCLI/<VERSION>` even in the Rust binary. These are
  historical IDs, not Python parity — they must simply never change casually.
- Tool schemas, descriptions (`kimi-agent/src/tools/desc/`), approvals, prompts,
  and compaction behavior are this repo's own canonical definition now.

Breaking any of these requires a deliberate wire-protocol version bump, not a
casual edit. **Versioning**: the Rust workspace owns its version
(`core/Cargo.toml`); it is no longer tied to any Python release numbering.

## Build and test commands

Single toolchain — **run cargo from `core/`**. Dev machine is Windows; commands
assume Git Bash.

```sh
cargo build -p kimi-agent          # agent binary → core/target/{debug,release}/kimi-agent
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
# native GUI (from core/):
cargo build -p kimi-gui && ./target/debug/kimi-gui --agent-bin ./target/debug/kimi-agent
# (or set KIMI_AGENT_BIN; remaining args are forwarded to the agent verbatim)

# terminal UI (from core/) — run inside a real terminal, one session per invocation:
cargo build -p kimi-tui && ./target/debug/kimi-tui -w /some/dir
# (agent binary resolved the same way: --agent-bin flag, KIMI_AGENT_BIN,
#  sibling executable, PATH)

# headless agent with no UI:
./target/debug/kimi-agent
```

## Module map

**`core/kimi-agent/src/`** — `cli/` (arg parsing; `info`, `mcp` subcommands),
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

**`core/kosong/src/`** — `message.rs` (canonical message types), `chat_provider/`
(Kimi, Echo, ScriptedEcho — the latter two for tests), `tooling/`, `generate.rs`
(streaming merge + tool-call orchestration).

**`core/kaos/src/`** — `Kaos` trait + `LocalKaos`, task-local `current` override,
`KaosPath` (canonical/expanduser, no symlink resolution), `cached.rs` (glob cache
+ bounded write-undo history), Python-`os.stat`-shaped results (naming is
historical).

**`core/wire-client/src/`** — the shared frontend kit every frontend builds on:
- `lib.rs` — the wire-protocol client used to spawn and drive a `kimi-agent`
  subprocess: JSON-RPC framing, the `Inbound` classification (event /
  reverse-request / response / exit / protocol error), stderr tail capture,
  request-id generation, graceful shutdown. UI-toolkit-free: callers pass a
  `wake` hook (egui: `request_repaint`; the TUI: a channel send) and choose
  console inheritance (`spawn`) or `CREATE_NO_WINDOW`
  (`spawn_without_console`).
- `launch.rs` — agent-binary resolution shared by both mains (`--agent-bin`
  flag → `KIMI_AGENT_BIN` → sibling executable → `PATH`).
- `transcript.rs` — folds wire events into renderable blocks (moved here from
  kimi-gui so any frontend gets identical state).
- `session_list.rs` — background listing of resumable sessions under `~/.kimi`.

**`core/kimi-tui/src/`** — the ratatui terminal frontend. One conversation per
invocation (a TUI owns the whole terminal). `main.rs` (event loop over one mpsc
channel fed by crossterm input and the client's wake hook; overlays for
approvals and resume; status bar), `agent.rs` (protocol state machine:
initialize → replay → ready/running/failed, request-id correlation, approval
answering), `input.rs` (single-line editor, char-boundary safe),
`render.rs` (blocks → pre-wrapped styled rows with width-aware wrapping;
scrollback is index arithmetic on the row list).

**`core/kimi-gui/src/`** — `app.rs` (top-level wiring, shortcuts, overlays,
`focus_owner()`), `session.rs` (one agent child + transcript + approval UI per
tab), `render.rs` (transcript block widgets), `theme.rs` (light/dark/Kimi
palettes, moon spinner, `BarStyle`), `palette.rs` (command palette), `os.rs`
(open-in-default-app); transcript/session-list moved to `wire-client`.

## Code style

- **Rust**: rustfmt clean; community naming/concurrency/error-handling conventions
  (anyhow/thiserror); write detailed comments for public APIs and tricky
  implementations; sub-directory `AGENTS.md` notes for key modules. No `unsafe`.
- **Comments/docs language**: English (the historical rewrite plan in `core/_` is
  Chinese).

## Testing strategy

- Rust tests live in `core/{kimi-agent,kosong,kaos,kimi-gui,kimi-tui,wire-client}/`
  (integration dirs and inline `#[cfg(test)]` units); E2E wire tests use
  ScriptedEcho and mock HTTP (`wiremock`, `axum`).
- Wire/data compatibility is verified read-only against real `~/.kimi` data in
  tests — never mutate user session dirs in tests or tooling.

## Build & release

`cargo build -p kimi-agent` / `-p kimi-gui` / `-p kimi-tui` produce
self-contained binaries (macOS/Linux/Windows).

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
  machine; the shell tool may start in a different cwd — always `cd` into
  `core/` explicitly.
- When changing wire-visible behavior, update the tests and the relevant
  sub-tree `AGENTS.md` in the same change. If the change breaks wire/data
  compatibility, bump the protocol version deliberately.
