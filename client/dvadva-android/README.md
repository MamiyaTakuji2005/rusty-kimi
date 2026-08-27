# dvadva-android

A phone frontend for a **dvadva-agent** session: a dumb TCP client that
attaches to a `dvadva-bridge` daemon over the phone's **existing WireGuard
tunnel** and speaks the wire protocol (JSON-RPC over the bridge framing).
Kotlin + Jetpack Compose. One conversation per app run — a phone owns the
whole screen, the way `dvadva-tui` owns a terminal.

It deliberately does **not** embed or manage a WireGuard tunnel: the stock
WireGuard app (or any tunnel the user already runs) provides the private
network, and this app is just the client at the far end of it. No VpnService,
no foreground service, `INTERNET` as its only permission.

## What it is, in terms of the repo

This is a Kotlin port of the pieces of `client/wire-client` a phone needs:

| Rust source | Kotlin twin | Notes |
|---|---|---|
| `bridge.rs` | `proto/Bridge.kt` | BRIDGE1 framing, 64 KiB cap, probe / list_sessions |
| `lib.rs` (`WireClient::connect_tcp`, `classify_line`) | `proto/WireClient.kt`, `proto/Wire.kt` | handshake + reader coroutine; `Inbound` sealed interface mirrors the Rust enum |
| (`dvadva_agent::wire` types) | `proto/Wire.kt` | the `{type, payload}` envelope and its messages |
| `wire/protocol.rs` | `proto/Protocol.kt` | `1.3` gate, `check_peer` |
| `transcript.rs` | `session/Transcript.kt` | simplified fold — phone-appropriate, `transcript.rs` stays the reference |
| `session_list.rs` (remote half) | `proto/Bridge.kt` | `list_sessions` over its own short-lived connection |

The golden-vector tests in `app/src/test` assert the same literal frames the
Rust test suites assert; they are this port's half of the drift guard the repo
already runs between `wire-client` and `dvadva-bridge`. **Change both sides or
neither.**

## Server side

On the machine the WireGuard tunnel already reaches (the desktop the phone
proxies through), run the bridge bound to the tunnel interface:

```sh
./dvadva-bridge remote --listen 10.7.0.1:9000   # the WireGuard interface IP
```

Only tunnel peers can route to that address; no ssh `-L`, no public exposure.
Sessions that pass no `-w` land in the daemon user's home directory (which is
why the app's work-dir field is optional and the paths it names are *the far
machine's*).

## Building

Toolchain: JDK 17+ (21 tested), Android SDK with Build-Tools 36, AGP 9.0 /
Gradle 9.1+ / Kotlin 2.3 — pinned in `gradle/libs.versions.toml`.

The Gradle **wrapper jar and scripts are not committed** (they are binary and
generated). First sync:

- **Android Studio**: open `client/dvadva-android` and let Studio import the
  Gradle project (it will offer to configure a Gradle distribution), or
- **CLI**: with a local Gradle ≥ 9.1 installed, run once inside this
  directory:

  ```sh
  gradle wrapper --gradle-version 9.1.0   # generates gradlew + the wrapper jar
  ./gradlew :app:assembleDebug
  ```

Unit tests (pure JVM, no device needed — the protocol layer has no Android
dependencies):

```sh
./gradlew :app:testDebugUnitTest
```

## Known limitations (by design, for now)

- **The agent dies with the connection.** Detaching without killing the agent
  is Phase 2 of `PLAN-detached-agent.md`. Until then: a dropped tunnel kills
  the running turn, and the app's recovery is resume-by-session-id + `replay`
  from the resume list. Do not paper over this locally — the fix belongs to
  the agent.
- Android Doze can idle the socket while backgrounded. If that becomes
  annoying, the fix is a plain foreground service, *not* VPN anything.
- The fold renders briefs only: diffs/todos/shell blocks show their one-line
  summary. Rich rendering can be ported from `transcript.rs` on demand.
