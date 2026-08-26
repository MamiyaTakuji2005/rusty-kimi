# Remote access — 2-daemon plan

Status: **implemented** (see `remote/AGENTS.md` for the current module map;
this file remains as the design record). Read it before changing anything
in `remote/` or `wire-client`'s transport: it captures *why* the pieces are
shaped the way they are.

## Goal

Drive a `kimi-agent` running on a **remote machine** from the local
`kimi-tui` / `kimi-gui`, with **zero changes to `kimi-agent`'s wire surface**
(protocol v1.2 stays untouched). Two relay daemons — one local, one remote —
carry the existing Wire protocol over the network as an **opaque byte stream**.

## Why it works (key insight)

The Wire protocol is transport-agnostic: the server reads newline-delimited
JSON from stdin and writes to stdout (`server/kimi-agent/src/wire/server.rs`);
the client does the same against a child process's pipes
(`client/wire-client/src/lib.rs`). The daemons are **dumb byte relays** — no
parsing, no state, no version knowledge. Close propagation is the only
semantics: socket close → agent stdin EOF → agent exits; agent exit → socket
close. A dropped tunnel mid-turn already degrades gracefully (server resolves
pending approvals as Reject and exits on stdin close).

Interim proof it works today: `kimi-tui --agent-bin ssh user@host kimi-agent -w /remote/dir`
already drives a remote agent over an ssh tunnel (`WireClient::spawn` takes any
binary). The daemons upgrade that into a persistent, reusable, multiplexed
connection instead of a per-tab ssh hack.

## Architecture

```
local:  kimi-tui/gui ─stdio─▶ kimi-bridge local ──TCP/SSH tunnel──▶ kimi-bridge remote ─stdio─▶ kimi-agent
                              (loopback listen)    (network leg)    (spawns kimi-agent)   (unchanged)
```

- **Remote daemon**: listens on a TCP port; per connection spawns `kimi-agent`
  with caller-chosen args (`-w`, `--config`, `--session`), pipes its stdio,
  relays bytes both ways.
- **Local daemon**: listens on 127.0.0.1 (or a Unix socket); each frontend
  connects to it; it holds the upstream connection to the remote daemon and
  relays bytes.

## Design decisions (settled)

- One crate: `remote/kimi-bridge`; one binary with `local` / `remote`
  subcommands (or two bins — pick the simpler). Tokio only, keep it dumb.
- Pure byte relay — the daemons must never parse the wire.
- **Security**: never expose on raw internet — the wire carries shell commands
  and approval prompts with no auth/TLS. Deploy behind `ssh -L` with both
  daemons bound to loopback, or add TLS + token inside the daemons.
- One agent per connection (matches the GUI's one-agent-per-tab model).
- Session/persistence (`~/.kimi`) stays on the remote box; the agent resolves
  its config, keys, skills, MCP there. Local config is not shared.

## Code changes (in order)

1. **Scaffold** `remote/kimi-bridge` as a workspace member (add to root
   `Cargo.toml` members). Deps: tokio (+ clap/serde for args) only.
2. **Relay core**: accept loop + spawn `kimi-agent` with piped stdio +
   bidirectional copy (`tokio::io::copy_bidirectional`) + close propagation +
   agent stderr capture/forwarding.
3. **`wire-client` transport refactor** (`client/wire-client/src/lib.rs`):
   factor `spawn_inner`'s reader/writer/`classify_line` plumbing behind a small
   duplex-stream abstraction; add `WireClient::connect(stream)` for sockets,
   keep `spawn()` for local child processes. `Inbound` classification,
   request-id generation, shutdown semantics stay unchanged.
4. **Frontend wiring**: `kimi-tui` + `kimi-gui` accept a remote endpoint
   (`--remote host:port` or `KIMI_REMOTE`) that yields a connected `WireClient`
   instead of a spawned child.
5. **Remote session listing**: the resume menu reads `~/.kimi` in-process
   (`client/wire-client/src/session_list.rs`). For remote, the remote daemon
   answers a listing query using `kimi_agent::session::Session::list` (a
   library call — no server change). Local listing stays as-is.
6. **Tests**: loopback e2e — remote daemon on localhost + scripted/real agent;
   verify prompt → events → approvals round-trip and close propagation.
7. **Docs**: AGENTS.md layout (remote/ no longer "planned"), README quick
   start, mark this file done.

## Explicitly out of scope

- **True mid-session attach** (joining a live agent): the wire server is
  single-client, picks the session at spawn time, and `initialize` rejects
  while a turn runs. Needs server changes + a protocol version bump —
  deliberately a separate project.
- **Shared `wire-protocol` crate extraction** (client depends on
  `kimi_agent::wire` + `Session::list`): already deferred — do not start as a
  side quest.

## Implementation record

Landed as `remote/kimi-bridge` (one crate, one binary, two subcommands) plus
a stream transport in `wire-client`:

- **Bridge framing** (`remote/kimi-bridge/src/proto.rs`, mirrored client-side
  in `client/wire-client/src/bridge.rs`): every connection starts with one
  `BRIDGE1 <json>` line — `{"op":"spawn","args":[…]}` or
  `{"op":"list_sessions"}` — and the daemon answers with exactly one reply
  frame (the spawn ack doubles as the spawn-failure channel). Everything
  after that line is opaque wire-protocol bytes. A dev-dependency test in
  `wire-client` pins the two framings byte-for-byte.
- **Close propagation** is explicit: two copy tasks per connection; client
  EOF → agent stdin shutdown, agent EOF → exit trailer → socket write
  shutdown. Grace period then kill for a stuck agent.
- **The exit trailer** (added after the first round of VPS review): once the
  agent's stdout has closed, the daemon appends one last `BRIDGE1` frame
  with the exit status and the agent's stderr tail, then half-closes. It is
  the only byte either daemon writes after the header, and it does not dent
  the no-parsing rule — the agent's stream is provably finished by then.
  Without it the local and remote transports are not equivalent: a local
  agent that dies on startup reports why (`wire_client` keeps its stderr
  tail), a remote one used to report only "connection closed".
- **The work-dir default**: a frontend on another OS cannot name a path that
  exists on the agent's machine, so the daemon decides. `--work-dir`, else
  the daemon user's home; prepended as `-w` only when the caller named none.
  This is why kimi-gui's `+` opens no folder picker over a remote bridge.
- **`WireClient`** gained `connect_tcp()` (TCP + handshake + stream IO) next
  to `spawn()`/`spawn_without_console()`; the reader/writer/classify plumbing
  is shared behind `start_io`, and stream EOF surfaces as
  `Inbound::AgentExited("remote connection closed")` — no new enum variant,
  so no frontend changes for the disconnect path. The handshake is bounded
  at every step (connect timeout, read timeout, frame size cap): frontends
  run it on the thread that draws their UI, and a daemon that accepts and
  then says nothing must not freeze them.
- **Frontends**: `--remote <host:port>` / `$KIMI_REMOTE` (stripped from
  agent args by `AgentLaunch`); `kimi-tui`/`kimi-gui` branch to
  `connect_tcp`, and the resume menu lists through the daemon
  (`spawn_remote_session_listing`) instead of local `~/.kimi`.
- **Session listing** on the daemon reuses `kimi_agent::session::Session`
  as a library call — no server change, `~/.kimi` untouched.

### Second round: making it a thing you can actually run

- **`~/.kimi/bridge.toml`**, one file with two disjoint halves: `[serve]`
  (read by `kimi_bridge::config`, so a systemd unit is a bare
  `kimi-bridge remote`) and `[[remotes]]` (read by `wire_client::remotes`:
  name, endpoint, optional tunnel command, default flag). Disjoint because
  the daemon must not depend on the frontend kit, and a shared type would be
  exactly that dependency. Not a section in `config.toml`: the agent rewrites
  that file from its own struct and would drop what it does not know.
- **`--remote` takes a name or a `host:port`.** `launch.rs` stays a pure
  argument parser; `remotes::resolve` does the lookup and produces one good
  error listing the configured names.
- **A `version` op** on the bridge protocol. There is no long-lived
  connection to observe — every session dials its own — so a UI indicator
  needs something cheap to poll. It spawns nothing and reads no disk, and it
  answers even when the agent binary is missing.
- **`wire_client::tunnel`** runs the ssh process as a managed child (spawn,
  liveness, stderr tail, kill). It is not a shell: the command is split, not
  interpreted, and the child gets no console — so tunnel commands must be
  non-interactive, and what ssh complains about reaches the UI through the
  stderr tail.
- **Sessions became per-tab** in kimi-gui (`SessionTarget`): `--remote` now
  says where the *first* tab opens rather than switching the whole window,
  local and remote tabs coexist, `+` follows the active tab's machine, and
  the resume menu lists (and resumes onto) whichever machine is in view.
  `remote_link.rs` folds tunnel state and probe results into the three-state
  button.

## Verification gate

- `cargo fmt --check`, `cargo test --workspace` from the repo root; zero
  clippy warnings in `remote/` and `wire-client`.
- Loopback e2e (`remote/kimi-bridge/tests/`): frame handling, arg passing,
  the work-dir default and its override, opaque relay, close propagation
  both directions, the exit trailer (status + stderr tail), spawn failure
  surfaces, local-daemon forwarding (13 tests); client handshake tests in
  `wire-client` — refused spawn, unreachable daemon, silent daemon, a peer
  that never frames, trailer→`AgentExited`, and the framing drift-guard.
- Manual smoke (done, Windows): `kimi-bridge remote` + the real `kimi-agent`
  — spawn ack, `initialize` round-trip with full server capabilities,
  `list_sessions` against real `~/.kimi`, clean exit on disconnect.
- Wire protocol version stays 1.2; no `~/.kimi` format changes.

## Deployment (VPS)

```sh
# on the VPS (build there, or copy target/release binaries):
./kimi-bridge remote --listen 127.0.0.1:9000   # agent resolved: --agent-bin /
                                               # $KIMI_AGENT_BIN / sibling / PATH
                                               # work dir: --work-dir / $HOME

# on the local machine (one terminal, kept open):
ssh -N -L 9000:127.0.0.1:9000 user@vps

# from any local terminal (the optional local daemon is not needed when
# ssh -L lands directly on the remote daemon):
./kimi-tui --remote 127.0.0.1:9000             # no -w: the daemon defaults
./kimi-gui  --remote 127.0.0.1:9000            # paths resolve on the VPS
```

The `local` subcommand (`kimi-bridge local --upstream 127.0.0.1:9000`) is
for deployments where frontends should not know the upstream address (it
forwards both ops verbatim); `ssh -L` directly onto the remote daemon is
the simplest secure setup.
