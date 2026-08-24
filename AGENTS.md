# AGENTS.md

Guidance for AI coding agents working in this repository. Read this before making changes.
`CLAUDE.md` (repo root) and the sub-tree `AGENTS.md` files listed below carry overlapping,
more detailed rules — when they conflict with reality, trust the code.

## Project overview

**rusty-kimi** runs the *Kimi Code CLI* shell UI (Python) on a **Rust agent core**, connected
over the **Wire protocol** (JSON-RPC over stdio). It vendors the full source of two Apache-2.0
Moonshot AI projects (both discontinued upstream), plus one new fork-authored frontend:

| Path | What it is | Origin |
|------|------------|--------|
| `core/` | Rust workspace: agent core, LLM/OS abstractions, native GUI | fork of MoonshotAI/kimi-agent-rs |
| `cli/` | Python shell UI (TUI), session mgmt, hooks, MCP OAuth, wire bridge | fork of MoonshotAI/kimi-cli |
| `core/kimi-gui/` | **new in this fork**: native egui frontend for the wire protocol | fork-authored |

```
shell UI (cli/, Python) ──Wire JSON-RPC / stdio──▶ kimi-agent (core/, Rust)
kimi-gui  (core/, Rust) ──Wire JSON-RPC / stdio──▶ kimi-agent (core/, Rust)
```

The fork's active work is *reconciling an older Rust core with a newer Python shell*: the
vendored Python `kimi-cli` is at 1.47.0 while the Rust workspace is at 1.8.0. Recent commits
port features across (SubagentEvent mirroring, user-invokable `/fork`, flattened
StrReplaceFile schema, Git Bash shell preference + 30 s per-call timeout floor).

## Repository layout

```
core/                  Rust workspace (run cargo here)
  kimi-agent/          main crate — wire-only agent server (bin: kimi-agent)
  kosong/              LLM abstraction (messages, tooling, chat providers)
  kaos/                OS abstraction (LocalKaos, path semantics)
  kimi-gui/            egui frontend (bin: kimi-gui)
  _/                   historical rewrite PROMPT.md / PLAN.md (Chinese; context only)
cli/                   Python workspace (run make/uv here)
  src/kimi_cli/        the shell UI application
  packages/kosong/     Python LLM abstraction (workspace package)
  packages/kaos/       Python OS abstraction, package name `pykaos`
  packages/kimi-code/  thin alias package: `kimi-code` script → kimi_cli
  vis/                 web tracing visualizer (React 19 + Vite + Tailwind)
  kimi.spec            PyInstaller spec for standalone binaries
```

Per-module notes live in sub-tree `AGENTS.md` files — read them before touching those areas:
`core/AGENTS.md` (sync contract, workspace overview), and under `core/kimi-agent/src/`:
`cli/`, `soul/`, `wire/`, `tools/`, `skill/`; plus `core/kosong/src/AGENTS.md`,
`core/kaos/src/AGENTS.md`.

## Architecture

Two-tier system. The **Python side never executes LLM steps**; the **Rust side owns the
agent loop**.

| Concern | Python (`cli/`) | Rust (`core/kimi-agent/`) |
|---|---|---|
| TUI / shell prompt, slash commands | ✓ | |
| Session state, hooks engine, MCP OAuth | ✓ | |
| Wire protocol framing | both | both |
| LLM API calls, agent loop/step | | ✓ |
| Context compaction | | ✓ |
| Tool dispatch + built-in tools | | ✓ |
| Skills/flows, prompts | | ✓ |
| MCP tool calls | Python manages clients | Rust dispatches |

- **Python runtime path**: `kimi_cli/cli` (typer entry) → `app.py` (`KimiCLI`) →
  `wire/client.py` (`WireClient`) spawns the agent subprocess and speaks JSON-RPC;
  `soul/remote.py::run_remote_soul` drives one turn through the external agent, attaching
  the UI loop to a fresh per-turn `Wire`. The agent process owns session persistence
  (writes its own `wire.jsonl` / `context.jsonl` under `~/.kimi`) — the Python side does
  **not** record a local `WireFile` in remote mode.
- **Rust runtime path**: `cli/` (arg parsing; subcommands `info`, `mcp`) → `app.rs`
  (`KimiCLI::create`) → `soul/kimisoul.rs` (loop, steps, flow/Ralph runner) → `tools/`
  dispatch; `wire/server.rs` exposes the stdio JSON-RPC server.
- **Wire protocol**: version negotiation via `initialize`; the Python client falls back to
  legacy mode (pre-1.1, no handshake) if the agent returns `-32601`. Current constants
  **differ**: Python `WIRE_PROTOCOL_VERSION = "1.10"`, Rust `"1.2"` (legacy `"1.1"`).
- **Data layout under `~/.kimi`** must stay identical on both sides: `config.toml`,
  `kimi.json`, `mcp.json`, session dirs with `context.jsonl` + `wire.jsonl`.

### Python cannot run standalone — vestigial code, do not rebuild it

Local LLM execution was removed from Python. The remnants are intentionally minimal:

- `soul/kimisoul.py` — stub; constructing `KimiSoul` raises `RuntimeError`. Exists only so
  `isinstance` checks resolve (always `False`; the active soul is `RemoteSoul`).
- `soul/compaction.py` — reduced to `estimate_text_tokens`. Rust owns compaction.
- `KimiToolset.load_tools` silently skips any tool path under `kimi_cli.tools.`;
  execution is delegated to the Rust agent over the wire.
- Removed entirely (**do not re-add**): `soul/slash.py`, `soul/btw.py`, and the `skill/`,
  `prompts/`, `agents/`, `skills/` package data — all owned by the Rust agent now.
- The Python `kosong` chat providers remain as library code but are unused in the main
  execution path.

## Build and test commands

Two toolchains — **run cargo from `core/`, make/uv from `cli/`**. Dev machine is Windows;
commands below assume Git Bash. For the Python shell UI on Windows also set `PYTHONUTF8=1`.

### Rust (`core/`)

```sh
cargo build -p kimi-agent          # agent binary → core/target/{debug,release}/kimi-agent
cargo build -p kimi-gui            # native GUI frontend
cargo test                         # whole workspace
cargo test -p kimi-agent <name>    # single test
cargo fmt                          # formatting is enforced (see git history)
cargo clippy --workspace --all-targets
```

Workspace rules: edition **2024**, `unsafe_code = "deny"`, `clippy::all = "warn"`.
Prefer async I/O (tokio); avoid blocking locks in async contexts.

### Python (`cli/`)

```sh
make prepare    # one-time: uv sync --frozen --all-extras --all-packages (+ prek hooks)
make check      # ruff check + ruff format --check + pyright (strict) + ty (non-blocking)
make format     # ruff autofix + format across all workspace packages
make test       # all suites (see caveat below)
# kosong / pykaos package tests:
uv run --project packages/kosong --directory packages/kosong pytest --doctest-modules -vv
uv run --project packages/kaos  --directory packages/kaos  pytest tests -vv
```

**Caveat — removed in this fork:** `cli/tests/`, `cli/tests_e2e/`, `cli/tests_ai/`,
`cli/scripts/`, and `cli/sdks/` no longer exist. Makefile targets referencing them
(`test-kimi-cli`, `ai-test`, `gen-changelog`, `gen-docs`, `build-bin`, the
`scripts/inject_build_sha.py` / `scripts/build_vis.py` steps) are **vestigial upstream
targets** and will fail as-is. Only the `packages/kosong` and `packages/kaos` pytest suites
remain. The inherited `.github/workflows/*` in `cli/` also reference these removed paths;
there is no repo-root CI.

Cross-language E2E (`KIMI_E2E_WIRE_CMD=... uv run pytest tests_e2e`) is likewise gone with
that directory — parity is covered by Rust tests under `core/*/tests`, including wire-mode
E2E using the ScriptedEcho provider and `wiremock`/`axum` mock services.

### Running the app

```sh
# shell UI on the Rust core (from cli/):
PYTHONPATH=src KIMI_AGENT_BIN="../core/target/debug/kimi-agent" python -m kimi_cli
# Windows/PowerShell: quote the path and set PYTHONUTF8=1.
# The banner shows "(remote / wire)" when running on the Rust core.

# native GUI (from core/):
cargo build -p kimi-gui && ./target/debug/kimi-gui --agent-bin ./target/debug/kimi-agent
# (or set KIMI_AGENT_BIN; remaining args are forwarded to the agent verbatim)

# vis web visualizer dev servers (from cli/): make vis-back (uvicorn :5495) / make vis-front (vite)
```

Without `KIMI_AGENT_BIN` the Python process has no agent loop and cannot function.

## Sync contract (must-follow)

Rust and Python must stay in lockstep on **all external behavior**. When behavior
conflicts, **Python (`cli/src/kimi_cli`, `cli/packages/*`) is the source of truth**, and
both sides should change together:

- Wire protocol: envelopes, `type` strings (match Python class names), error codes.
- `kosong.message` ↔ `kimi_cli.wire.types` schemas and serde (e.g. `Message.content`:
  single `TextPart` serializes to a JSON string, otherwise an array of parts).
- `~/.kimi` config/session/context/wire JSONL formats.
- Tool schemas, descriptions (`kimi-agent/src/tools/desc/` must match Python), approvals,
  prompts, compaction.
- Tool identifiers remain `kimi_cli.tools.*`; wire identity stays "Kimi Code CLI" /
  `KimiCLI/<VERSION>` even in the Rust binary.
- Internal IDs/names that appear on the wire must remain stable.

**Versioning:** the documented rule is that the Rust workspace version must exactly match
the Python `kimi-cli` version (`MAJOR.MINOR.PATCH`). Current state **diverges**
(core = 1.8.0, cli = 1.47.0); treat closing such gaps (version *and* wire-protocol
constants *and* behavior) as part of ongoing work.

Note: the Chinese user docs (`cli/docs/zh/`) referenced by older notes were removed in this
fork; the code is the practical reference now.

## Module map

**`core/kimi-agent/src/`** — `cli/` (typer-equivalent parsing; `info`, `mcp` subcommands),
`app.rs` (`KimiCLI::create` wiring), `soul/` (`kimisoul.rs` loop; `context.rs` JSONL
history with checkpoints/rotations; `approval.rs` queue + YOLO; `compaction.rs`;
`toolset.rs` dispatch + MCP bridge), `wire/` (`types.rs`, `serde.rs`, `file.rs` JSONL
persistence, `server.rs` stdio server, `channel.rs` merge logic), `tools/` (Shell;
`file/` Read/Write/Replace/Glob/Grep/ReadMedia; `web/` SearchWeb/FetchURL; todo;
`multiagent/` Task + CreateSubagent; dmail; think; `test.rs` plus/compare/panic),
`skill/` (skill discovery, mermaid/d2 flows), `config.rs`/`metadata.rs`/`session.rs`/
`share.rs` (persistence), `mcp.rs` (rmcp client), `prompts/`, `skills/`, `agents/`.

**`core/kosong/src/`** — `message.rs` (canonical message types), `chat_provider/`
(Kimi, Echo, ScriptedEcho — the latter two for tests), `tooling/`, `generate.rs`
(streaming merge + tool-call orchestration).

**`core/kaos/src/`** — `Kaos` trait + `LocalKaos`, task-local `current` override,
`KaosPath` (canonical/expanduser, no symlink resolution), Python-`os.stat`-shaped results.

**`cli/src/kimi_cli/`** — `app.py`, `cli/` (typer commands: `login`, `logout`, `vis`,
hidden `__background-task-worker`; groups `mcp`, `info`, `export`), `ui/` (shell TUI),
`wire/` (`client.py` bridge, `server.py`, `root_hub.py`, `types.py`, `protocol.py`),
`soul/` (`remote.py` + stubs), `session*.py`, `hooks/`, `auth/`, `approval_runtime/`,
`subagents/` (remote-only; local execution raises), `mcp_oauth.py`, `telemetry/`,
`vis/` (FastAPI backend), `background/`, `notifications/`, `agentspec.py`, `llm.py`
(capabilities), `config.py`/`constant.py`/`metadata.py`/`share.py`, `deps/`.

## Code style

- **Rust**: rustfmt clean; community naming/concurrency/error-handling conventions
  (anyhow/thiserror); write detailed comments for public APIs and tricky implementations;
  sub-directory `AGENTS.md` notes for key modules. No `unsafe`.
- **Python**: ruff (line length 100; rules `E,F,UP,B,SIM,I`), `ruff format`,
  pyright **strict** on `src/kimi_cli/**` (target Python 3.14; `requires-python >=3.12`),
  `from __future__ import annotations`, fully typed (`py.typed`). `ty` runs non-blocking.
  pytest config: `asyncio_mode = auto`.
- **Comments/docs language**: English (the historical rewrite plan in `core/_` is Chinese).

## Testing strategy

- Rust tests live in `core/{kimi-agent,kosong,kaos}/tests/` (integration) and inline
  `#[cfg(test)]` units; E2E wire tests use ScriptedEcho and mock HTTP (`wiremock`, `axum`).
  Run with `cargo test`; keep coverage aligned with Python behavior.
- Python: package suites only (`packages/kosong` — includes doctests — and
  `packages/kaos`); the main CLI test dirs were removed in this fork (see caveat above).
- Wire/data compatibility is verified read-only against real `~/.kimi` data in tests —
  never mutate user session dirs in tests or tooling.

## Build & release

- Rust: `cargo build -p kimi-agent` produces a single self-contained binary (no Python
  dependency; macOS/Linux/Windows).
- Python distribution targets (`make build`, `make build-bin*` via PyInstaller `kimi.spec`,
  `uv build` per package, vis bundling) are inherited from upstream but currently depend
  on removed `scripts/`; treat them as broken until the paths are restored.
- `cli/flake.nix` provides a Nix dev environment (optional).

## Security considerations

- `cli/SECURITY.md` (inherited): only the latest version is supported; vulnerabilities
  were reported via the upstream MoonshotAI/kimi-cli security page.
- Approval gating: tools (Shell, WriteFile, StrReplaceFile, …) require user approval;
  YOLO mode auto-approves — be deliberate when enabling it.
- Shell tool on Windows prefers **Git Bash**; per-call timeout is floored at 30 s.
- MCP: config in `~/.kimi/mcp.json` (same schema both sides); Rust uses `rmcp` whose
  OAuth credential storage paths are **not compatible** with Python fastmcp token
  locations (known, accepted incompatibility). MCP clients must not auto-inject
  `mcp-session-id` headers (some standard servers reject them).
- `.claude/settings.local.json` contains local, machine-specific permission grants — it
  is not project configuration; do not treat its paths as portable.

## Misc conventions

- Git: single `main` branch; subtrees were vendored with per-subtree history preserved.
  Do not run `git commit/push/reset/rebase` unless explicitly asked.
- The repo lives at `C:\Users\MamiyaTakuji\.rusty-kimi\rusty-kimi` on this machine; the
  shell tool may start in a different cwd — always `cd` into `core/` or `cli/` explicitly.
- When changing wire-visible behavior, update both sides, their tests, and the relevant
  sub-tree `AGENTS.md` in the same change.
