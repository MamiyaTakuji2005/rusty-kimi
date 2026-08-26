# remote/ — relay daemons for remote access

`kimi-bridge` carries a `kimi-agent` wire connection across the network as
an **opaque byte stream**. The design record and rationale live in
[`PLAN.md`](PLAN.md) — read it first; this file is the module map.

## The one rule

**The daemons never parse the wire protocol.** The only thing either daemon
ever interprets is its own one `BRIDGE1` header line per connection
(`proto.rs`). Everything after that line — the frontend↔agent JSON-RPC
stream — flows through untouched. This is what keeps the bridge version-
independent from the wire protocol (still v1.2, unchanged by this crate).

Corollaries:

- No auth, no TLS, no wire knowledge in the relay path. Bind to loopback,
  cross the network through `ssh -L`. The binary prints this warning on
  startup on purpose.
- Close propagation is the only semantics: client half-close → agent stdin
  EOF → agent exits; agent exit → socket write shutdown → client sees EOF.
  One agent per connection; the connection's lifetime *is* the agent's.

## Layout

- `src/proto.rs` — the bridge framing: `BRIDGE1 <json>` request/reply
  frames, the 64 KiB line cap, and the leftover-preserving `read_line`
  (clients may pipeline wire bytes right after the header; those must stay
  buffered). Client-side twin: `client/wire-client/src/bridge.rs` — a
  dev-dependency test in `wire-client` fails if the two drift.
- `src/remote_daemon.rs` — runs on the agent machine. `spawn` op: spawn
  `kimi-agent` with the caller's args (header `args` — verbatim agent CLI
  args), ack, relay, wait-with-grace-then-kill. `list_sessions` op: the
  remote twin of `wire_client::session_list::list_all_sessions` (same
  `load_metadata` + `Session::list` reads, wrapped in a task so a listing
  panic can't kill the daemon). Agent stderr is forwarded to the daemon's
  stderr tagged `conn N`.
- `src/local_daemon.rs` — runs on the frontend machine, forwards frames
  verbatim and relays bytes; optional in the `ssh -L` deployment.
- `tests/bin/mock_agent.rs` — scripted stand-in for `kimi-agent` (echo /
  `say X` / `argv` / `die`, `MOCK-AGENT-EOF` on stdin EOF); it is a `[[bin]]`
  so `CARGO_BIN_EXE_kimi-bridge-mock-agent` works from `tests/e2e.rs`.
- `tests/e2e.rs` — loopback e2e over real TCP sockets; covers relay
  opacity, arg passing, close propagation in both directions, spawn
  failure frames, garbage headers, and both local-daemon ops.

## Conventions

- Tokio-only, `unsafe_code` denied via workspace. The workspace tokio
  feature set lacks `net` — the crate adds it locally in `Cargo.toml`.
- Agent-binary resolution mirrors `wire_client::launch` (flag →
  `KIMI_AGENT_BIN` → sibling → PATH) but is intentionally *not* shared:
  the daemon must not depend on the frontend kit.
- Reply frames are exactly one line. The `spawn` ack doubles as the spawn-
  failure channel (`{"ok":false,"error":…}`) so `WireClient::connect_tcp`
  can surface a missing agent binary as a connect error.
- `SessionEntry` (proto) and `ResumeEntry` (wire-client) are the same wire
  shape; `list_sessions` is read-only against the daemon's `~/.kimi` and
  must never write there.

## Testing

`cargo test -p kimi-bridge` from the repo root. The e2e suite binds
ephemeral loopback ports only; the `list_sessions` test asserts reply
*shape* (contents are the machine's own `~/.kimi`, possibly empty on CI).
