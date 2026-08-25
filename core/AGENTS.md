# Kimi Agent (Rust)

## Quick commands (cargo)

- `cargo build -p kimi-agent`
- `cargo test -p kimi-agent`
- `cargo test -p kosong`
- `cargo test -p kaos`
- `cargo test` (workspace)
- `cargo fmt`
- `cargo clippy --workspace --all-targets`

## Purpose and naming

Kimi Agent is a wire-only agent server (no Shell/Print/ACP UI) and lives in this
repository. The binary name is `kimi-agent`, and the wire identity it inherited
from the Python original is now a compatibility invariant:

- Wire metadata and user-agent still identify as "Kimi Code CLI" / `KimiCLI/<VERSION>`.
- Tool identifiers remain `kimi_cli.tools.*`.
- The CLI `about` string is "Kimi Agent, the Rust agent server."

These are historical IDs that must never change casually — existing `~/.kimi`
data and any wire client (including the archived TUI) depends on them.

## Compatibility contract (must-follow)

The Python tree is archived out of this repo; this workspace defines the
canonical behavior. The following are stability invariants, kept so existing
`~/.kimi` data stays readable and wire clients keep working:

- Wire protocol, message envelopes, `type` strings, error codes.
- `kosong.message` schema and serde behavior (e.g. `Message.content`
  string/parts rules).
- Config, metadata, sessions, and context JSONL formats under `~/.kimi`.
- Agent specs, prompts, skills/flows, tool schemas/descriptions, approvals,
  compaction.
- Providers and Kaos behavior (Kimi/Echo/ScriptedEcho, LocalKaos).
- Internal IDs and names that appear on the wire.

Breaking any of these needs a deliberate wire-protocol version bump, not a
casual edit.

## Versioning (must-follow)

The Rust workspace version (`Cargo.toml`) is this project's own version number.
It is no longer tied to the archived Python `kimi-cli` release numbering — bump
it by normal semver rules as the Rust system evolves.

## Rewrite constraints (from _/PROMPT.md and _/PLAN.md — historical)

- Three crates: `kimi-agent`, `kosong`, `kaos` (plus the fork-added `kimi-gui`).
- Rust edition 2024, async runtime `tokio`, serde, anyhow/thiserror, clap, reqwest.
- Only WireOverStdio UI; no Shell/Print/ACP UI.
- Full parity with the Python original for data formats and wire behavior was the
  rewrite goal; that parity is now the frozen baseline the invariants above
  describe.
- Tests are ported for core runtime/tools/wire; UI-only tests are omitted.

## Workspace layout

- `kimi-agent/` - main crate (binary: `kimi-agent`), wire-only agent server.
- `kosong/` - LLM abstraction layer (messages, tooling, providers).
- `kaos/` - OS abstraction layer (LocalKaos + path semantics).
- `wire-client/` - shared frontend client layer: spawns `kimi-agent`, speaks
  JSON-RPC over its stdio, folds events into transcript blocks, lists
  sessions under `~/.kimi` for resume.
- `kimi-gui/` - native egui frontend; the canonical wire client.

## CLI behavior (kimi-agent)

- Wire-only server; no UI selection flags.
- `--wire` exists but is hidden and ignored (legacy compatibility).
- No `--prompt`/`--command` because wire server does not accept an initial prompt.
- Subcommands: `info`, `mcp` only.
- Help text mirrors the original Python CLI; some MCP examples still show `kimi`.

## Known incompatibilities (historical, vs. the archived Python TUI)

- MCP OAuth credentials location differs from the Python fastmcp defaults; Rust
  uses `rmcp` credential storage paths.
- Options kept from the original: `--work-dir`, `--session`, `--continue`,
  `--config`, `--config-file`, `--model`, `--thinking/--no-thinking`, `--yolo`,
  `--agent`, `--agent-file`, `--mcp-config-file`, `--mcp-config`, `--skills-dir`,
  `--max-steps-per-turn`, `--max-retries-per-step`, `--max-ralph-iterations`.
- `help_expected` is enabled in clap, so every CLI arg must define help text.

## Major Rust modules (kimi-agent)

- `kimi-agent/src/cli/` - CLI parsing and subcommands (`info`, `mcp`).
- `kimi-agent/src/app.rs` - `KimiCLI::create` and runtime wiring.
- `kimi-agent/src/soul/` - core agent loop, approvals, compaction, context, toolset.
- `kimi-agent/src/wire/` - wire types, serde, WireOverStdio JSON-RPC server.
- `kimi-agent/src/tools/` - built-in tools (shell/file/web/todo/agent/fork/task/undo/dmail/think).
- `kimi-agent/src/skill/` - skills + flow parsing (mermaid/d2).
- `kimi-agent/src/config.rs`, `metadata.rs`, `session.rs`, `share.rs` - persistence.
- `kimi-agent/src/mcp.rs` - MCP config + loading (rmcp client).

## Wire protocol and data compatibility

- Wire protocol version negotiated via `initialize`, JSON-RPC over stdio.
- Data layout under `~/.kimi`: `config.toml`, `kimi.json`, session directories,
  context JSONL, wire JSONL — all stability invariants.
- `Message.content` string/parts serde rules are stability invariants.

## Providers and tools

- Providers: Kimi, Echo, ScriptedEcho (Echo variants used for tests).
- Kaos: LocalKaos only (SSH Kaos omitted for now).
- Built-in tools: Shell, Read/Write/Replace/Glob/Grep/ReadMedia, SearchWeb/FetchURL,
  SetTodoList, Agent, Fork, TaskList/Output/Stop, Undo, SendDMail, Think; test
  tools Plus/Compare/Panic.
- Tool descriptions live under `kimi-agent/src/tools/desc/` — this repo's
  canonical definitions (shape rules documented in `tools/AGENTS.md`).

## MCP integration

- MCP config: `~/.kimi/mcp.json`.
- Client: `rmcp` with stdio + HTTP transports, OAuth storage compatibility.
- CLI: `kimi-agent mcp add/list/remove/auth/reset-auth/test`.

## Tests

- Rust tests live under `kimi-agent/tests`, `kosong/tests`, `kaos/tests`,
  `kimi-gui` (inline).
- E2E tests cover wire-mode behavior using ScriptedEcho and mock services
  (`wiremock`, `axum`) — including a GUI-side wire client against a scripted
  agent.

## Conventions and runtime rules

- Prefer async I/O in runtime code; avoid blocking locks in async contexts.
- Keep prompts, schemas, error strings, and wire payloads stable per the
  compatibility contract above.
- If any behavior or documented interface changes, update this file and the
  corresponding tests in the same change.

## Pointers for future updates

- `_/PROMPT.md` defines the rewrite scope and compatibility constraints.
- `_/PLAN.md` records the parity plan and progress; treat it as historical context.
- The archived Python TUI lives in a separate private repo (`rusty-kimi-tui`);
  consult it only when a wire-behavior question needs the original reference.
