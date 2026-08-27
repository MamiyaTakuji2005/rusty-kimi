# AGENTS.md — dvadva-android

The Kotlin/Compose phone frontend. Read the root `AGENTS.md` first; this file
covers what is special here.

## What this is (and is not)

- A **dumb TCP client** to a `dvadva-bridge` daemon, reached over whatever
  VPN/tunnel the phone already runs. It never owns a tunnel, never spawns an
  agent process, and has exactly one permission (`INTERNET`).
- A **port, not a fork**: the protocol layer mirrors specific Rust files (see
  README table). When the Rust side changes a frame, a message shape, or an
  error string, this port changes with it, in the same change.

## Rules

- **Byte-fidelity over cleverness.** `proto/Bridge.kt`, `proto/Protocol.kt`,
  and `proto/Wire.kt` keep the Rust constants (`BRIDGE1`, `1.0`, `1.3`,
  64 KiB cap, 10 s connect / 15 s handshake) and the user-facing error
  strings. The golden tests in `app/src/test` pin the same vectors the Rust
  suites assert — treat a red golden test here exactly like the
  `framing_matches_the_daemon_side` test in `wire-client`.
- **No Android imports in `proto/`.** The protocol layer is pure Kotlin +
  kotlinx.serialization so it stays JVM-unit-testable without a device. UI
  code lives in `ui/`, session state in `session/`.
- **Frontend-generated agent flags are append-only** (root AGENTS.md). This
  app generates `-w <dir>` and `--session <id> [--session-workdir <dir>]`
  exactly like the other frontends; add new flags, never change these.
- **Ids from the agent are addresses; ids from the client are not.** Reverse-
  RPC ids (`ApprovalRequest`, `ToolCallRequest`) are safe to key state by;
  the app's own `client-N` request ids are unique only within one connection.
- Wire lines are **not** capped at 64 KiB — only bridge frames are. The
  reader loop uses unbounded `readLine`; the handshake reader does not.
- The transcript fold (`session/Transcript.kt`) is deliberately simpler than
  `wire-client/src/transcript.rs`; if a rendering question arises, the Rust
  file decides.
- Phase 2 of `PLAN-detached-agent.md` (detach/attach) will land on the agent
  side; when it does, the connection layer stays — resume becomes reattach in
  `SessionViewModel`, not a new transport.

## Build

JDK 17+, Android SDK (Build-Tools 36). The wrapper jar is not committed:
`gradle wrapper --gradle-version 9.1.0` once, or open in Android Studio.
`./gradlew :app:testDebugUnitTest` runs the protocol tests on the JVM.
Version pins and their compatibility notes live in `gradle/libs.versions.toml`.

This directory is **not** part of the Cargo workspace; `cargo` commands in the
root AGENTS.md do not see it, by design.
