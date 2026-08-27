# remote/ — relay daemons for remote access

`dvadva-bridge` carries a `dvadva-agent` wire connection across the network as
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
  EOF → agent exits; agent exit → one exit trailer, then socket write
  shutdown → client sees EOF. One agent per connection; the connection's
  lifetime *is* the agent's.

The one exception to "the daemon writes no bytes of its own after the
header" is that **exit trailer**: a final `BRIDGE1 {"ok":false,"error":…}`
frame carrying the agent's exit status and stderr tail, appended *after* the
agent's stdout has closed. It parses no wire and races nothing (the agent's
stream is provably over), and it is what gives a remote failure the same
diagnosis a local one gets from `wire_client`'s stderr tail — without it, a
bad work dir or a missing API key reaches the frontend as "connection
closed". `wire_client::bridge::exit_trailer` is the client-side twin.

## Layout

- `src/proto.rs` — the bridge framing: `BRIDGE1 <json>` request/reply
  frames, the 64 KiB line cap, and the leftover-preserving `read_line`
  (clients may pipeline wire bytes right after the header; those must stay
  buffered). Client-side twin: `client/wire-client/src/bridge.rs` — a
  dev-dependency test in `wire-client` fails if the two drift.
- `src/remote_daemon.rs` — runs on the agent machine. `spawn` op: spawn
  `dvadva-agent` with the caller's args (header `args` — verbatim agent CLI
  args), ack, relay, wait-with-grace-then-kill, exit trailer.
  `list_sessions` op: the remote twin of
  `wire_client::session_list::list_all_sessions` (same `load_metadata` +
  `Session::list` reads, wrapped in a task so a listing panic can't kill the
  daemon). Agent stderr is forwarded to the daemon's stderr tagged `conn N`
  *and* kept as a 20-line tail for the trailer.
  `Config::default_work_dir` prepends `-w` when the caller named none — the
  daemon owns that decision because a frontend on another OS cannot know a
  path that exists here. Prepended, not appended, so a caller's own `-w`
  still wins under clap's last-one-wins.
- Both daemons' accept loops survive a failing `accept()` (log, back off,
  continue) and give up only after 64 consecutive failures: a long-running
  daemon must not die of one transient error, and must not spin on a
  permanent one. Accepted sockets get `TCP_NODELAY` — wire traffic is many
  small writes, and Nagle turns streamed tokens into stutter over a real
  network.
- `src/local_daemon.rs` — runs on the frontend machine, forwards frames
  verbatim and relays bytes; optional in the `ssh -L` deployment.
- `src/config.rs` — the `[serve]` half of `~/.kimi/bridge.toml`, so a
  service unit can run a bare `dvadva-bridge remote`. The frontends' half
  (`[[remotes]]`) is read by `wire_client::remotes` and the two never share
  a type: **disjoint sections**, no dependency in either direction, unknown
  ones ignored. Not a section in `config.toml` — the agent rewrites that
  file from its own struct and would drop what it does not know.
- `tests/bin/mock_agent.rs` — scripted stand-in for `dvadva-agent` (echo /
  `say X` / `argv` / `die` / `fail X`, `MOCK-AGENT-EOF` on stdin EOF); it is
  a `[[bin]]` so `CARGO_BIN_EXE_dvadva-bridge-mock-agent` works from
  `tests/e2e.rs`.
- `tests/e2e.rs` — loopback e2e over real TCP sockets; covers relay
  opacity, arg passing, the work-dir default and its override, close
  propagation in both directions, the exit trailer (status + stderr tail),
  spawn failure frames, garbage headers, and both local-daemon ops.

## Conventions

- Tokio-only, `unsafe_code` denied via workspace. The workspace tokio
  feature set lacks `net` — the crate adds it locally in `Cargo.toml`.
- Agent-binary resolution mirrors `wire_client::launch` (flag →
  `KIMI_AGENT_BIN` → sibling → PATH) but is intentionally *not* shared:
  the daemon must not depend on the frontend kit.
- Reply frames are exactly one line. The `spawn` ack doubles as the spawn-
  failure channel (`{"ok":false,"error":…}`) so `WireClient::connect_tcp`
  can surface a missing agent binary as a connect error.
- The `version` op spawns nothing and touches no disk: it is what a UI
  polls to paint a connection indicator, so it must stay cheap enough to
  call on a timer and must answer even when the agent binary is missing.
- `SessionEntry` (proto) and `ResumeEntry` (wire-client) are the same wire
  shape; `list_sessions` is read-only against the daemon's `~/.kimi` and
  must never write there.

## Testing

`cargo test -p dvadva-bridge` from the repo root. The e2e suite binds
ephemeral loopback ports only; the `list_sessions` test asserts reply
*shape* (contents are the machine's own `~/.kimi`, possibly empty on CI).
