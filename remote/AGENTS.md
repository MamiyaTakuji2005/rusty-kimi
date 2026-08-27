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
- Close propagation is the only semantics, and what it propagates *to*
  depends on which op opened the connection.
  - `spawn` — the one-shot path: client half-close → agent stdin EOF →
    agent exits; agent exit → one exit trailer, then socket write shutdown →
    client sees EOF. One agent per connection; the connection's lifetime
    *is* the agent's.
  - `attach` — the supervised path: client half-close → the daemon closes
    *its* socket to the agent, which the agent reads as one client
    detaching. The agent keeps its turn, its context and its pid, and the
    next `attach` for that session lands on the same process.

  The two endings are told apart by **which direction ended first**, not by
  whether the copy ended cleanly: a killed frontend with bytes still in its
  receive buffer resets the connection, and a reset is an error on a read
  that is nonetheless a client leaving. Both endings half-close the client
  socket explicitly — a detach has to read as the end of a stream at the
  other end, not as a connection that broke — and only the agent-went-away
  ending writes a trailer first.

The daemon's one exception to "never parse the wire" grew a second member,
and it is still not the wire: on the `attach` path the daemon speaks the
agent's **token handshake** (`server/dvadva-agent/src/wire/listener.rs`),
which is a transport check settled before the first wire byte. Everything
after that line flows untouched, as before.

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
  Supervised agents are also given `--idle-timeout`, which is what keeps a
  long-lived daemon from collecting one process per session ever opened.
  `attach` op: find the agent hosting a session in the live-session registry
  (`dvadva_agent::live`) and dial it, or start one that is nobody's child in
  particular — `--listen`, its own process group, `kill_on_drop(false)`, and
  stderr to a log file beside the registry rather than to a pipe this daemon
  holds. It is found again by *pid*, because a brand-new session has no id
  until the agent mints one; the ack reports the id, which is the only way a
  caller who asked for a new session learns it. Three things it deliberately
  does not do: synthesize `--session` from the header's session id (that is
  the caller's argv to write), keep a stale registry entry from being served
  (it starts a fresh agent instead), or claim a diagnosis it does not have
  (only an agent *this connection* started has a log this daemon can quote).
  `list_sessions` op: the remote twin of
  `wire_client::session_list::list_all_sessions` (same `load_metadata` +
  `Session::list` reads, wrapped in a task so a listing panic can't kill the
  daemon), plus a `live` flag from the registry — and the live sessions the
  cold listing has no file for yet, because a live session nobody can see is
  a live session nobody can attach to. Agent stderr is forwarded to the
  daemon's stderr tagged `conn N` *and* kept as a 20-line tail for the
  trailer (on the `spawn` path; supervised agents log to a file instead,
  since a pipe dies with the daemon and the agent writes to stderr from
  places that panic if that write fails).
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
  service unit can run a bare `dvadva-bridge remote`. `agent_idle_timeout`
  lives here rather than in a frontend's hands because the daemon is where
  agents accumulate — it starts one per session anybody ever attaches to, and
  nothing else would end them. It is prepended to the agent's argv the way
  `-w` is, so a caller who states one still wins. The frontends' half
  (`[[remotes]]`) is read by `wire_client::remotes` and the two never share
  a type: **disjoint sections**, no dependency in either direction, unknown
  ones ignored. Not a section in `config.toml` — the agent rewrites that
  file from its own struct and would drop what it does not know.
- `tests/bin/mock_agent.rs` — scripted stand-in for `dvadva-agent` (echo /
  `say X` / `argv` / `pid` / `die` / `crash` / `stop` / `fail X`,
  `MOCK-AGENT-EOF` on stdin EOF); it is a `[[bin]]` so
  `CARGO_BIN_EXE_dvadva-bridge-mock-agent` works from `tests/e2e.rs`. Given
  `--listen` it does the listening half too — binds a port, mints a token,
  registers itself, and serves attachers — because the supervisor cannot be
  tested against something that only speaks stdio. It exits by itself after
  twenty seconds — several tests leave one running deliberately, that being
  the phase under test, and a stray one holds its own binary open, which on
  Windows fails the next *build* rather than the next test, with a message
  about file permissions that says nothing about where it came from. Keep
  that number well above the suite's own runtime (seconds) and well below how
  long anyone waits before building again.
- `tests/e2e.rs` — loopback e2e over real TCP sockets; covers relay
  opacity, arg passing, the work-dir default and its override, close
  propagation in both directions, the exit trailer (status + stderr tail),
  spawn failure frames, garbage headers, and both local-daemon ops. The
  supervisor half covers the whole of it: attach starts an agent and names
  the session, a client leaving finds the *same pid* on its way back in, a
  start that never listens is reported with its log, a crash ends the relay
  with a trailer, a stale entry starts a fresh agent instead of failing, and
  a listing marks the live one. Each supervising test gets a `share_dir` of
  its own (`Config::with_share_dir`), so a test run never advertises itself
  in the developer's `~/.kimi/live`.

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
  must never write there. (Reading the registry does prune entries that no
  longer answer — that is the registry's own contract, not the listing's.)
- The frame protocol's minor is the additive one: `attach`, `Reply::session`
  and `SessionEntry::live` are 1.1, and a 1.0 daemon refuses an `attach`
  frame as an unknown op. Bump `BRIDGE_PROTOCOL_VERSION` in **both** copies
  (`proto.rs` and `wire-client/src/bridge.rs`) — a test pins them together.

## Testing

`cargo test -p dvadva-bridge` from the repo root. The e2e suite binds
ephemeral loopback ports only; the `list_sessions` test asserts reply
*shape* (contents are the machine's own `~/.kimi`, possibly empty on CI).
