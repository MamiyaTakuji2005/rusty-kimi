# Detached agent — attach/detach plan

Status: **Phases 0, 1 and 2 done**, Phase 3 not started. Written 2026-08-27.

An agent now outlives its clients, and something that did not start it can
find it and join it. What is left is policy and frontends: the GUI and the
TUI still treat a closed connection as the end of a session, and nothing yet
uses `attach`.

Goal: a `dvadva-agent` that runs with no frontend attached, that several
frontends (GUI, TUI, a daemon, a script) can attach to and detach from while
it runs, without ending its turn or its process.

This spans all three tiers, so it lives at the root rather than beside one of
them. `remote/PLAN.md` is the sibling document for the transport work already
done; read it first — Phase 2 here rewrites one of its settled decisions
("one agent per connection").

---

## The shape of it

Attach is, at heart, an async replay pipe, and the read half already exists:

- `wire.jsonl` is a per-session append-only log of every wire message
  (`wire/file.rs`), written for every turn.
- `replay` (`wire/server.rs:462`) streams that whole file to a client.
- Both frontends already call it as step two of their handshake
  (`inkvizitor/src/session.rs:351`, `dvadva-tui/src/agent.rs:193`).

So "a client attaches and sees the conversation" is solved. It is currently
spelled *a fresh process replays a dead session* rather than *a live process
replays itself*. The work is in the write half and in lifetime.

Three things replay does not cover, and one prerequisite.

---

## Phase 0 — version every component (prerequisite) — **DONE**

Nothing below may ship before this. Not because the protocol change is large,
but because there was **no version negotiation at all**, in either direction,
and the first real mismatch would have been undiagnosable.

### The decision (settled)

**Component versions and protocol versions are separate numbers with separate
jobs, and both are reported everywhere.** A component version says *which
build* you are talking to; a protocol version says *whether you can talk to it
at all*. Only the second decides compatibility.

The alternative considered and rejected was one merged number per component
(`X.39` for the daemon, `X.13` for the TUI, compatible because `X` matches).
It reads well, and it is wrong here for two reasons:

- **It spends the minor on a build counter.** The case that pays for a version
  scheme is the additive one: a new optional message on the same major, which
  an older peer safely ignores. Expressing that needs a per-protocol minor. If
  the minor is a per-component tally, every additive change has to become a
  major bump, every major bump breaks all four binaries at once, and in
  practice you stop cutting them — at which point the number stops tracking
  reality.
- **There is no single protocol to be compatible *with*.** Three contracts run
  through this tree, and they do not move together: the **wire protocol**
  (frontend to agent, end-to-end, relayed opaquely), the **bridge frame
  protocol** (frontend to daemon), and the **agent CLI surface** (the frontend
  generates agent argv — `inkvizitor/src/app.rs:839` builds `-w` and
  `--session` — and ships it through the bridge to run on the far machine).
  The bridge sits between two of these and parses only one, so "the bridge is
  at X.39" cannot be true or false.

So: each protocol carries its own `major.minor` on its own clock; the crates
stay on the shared workspace version; every binary prints both.

**The rule**, identical for both protocols: same major required, minor is
additive only. A peer's *higher* minor is safe (ignore what you do not
recognize); a peer's *lower* minor means do not use what it predates.

### Findings that prompted it (2026-08-27, all now fixed)

1. **`initialize` declares, it does not negotiate.** The agent parses
   `params.protocol_version` into `InitializeParams` (`wire/jsonrpc.rs`) and
   never reads the field. The clients send `WIRE_PROTOCOL_VERSION` and never
   look at the `protocol_version` in the reply — `session.rs:340-352` checks
   only that a result arrived, then goes straight to `replay`.
   `AGENTS.md`'s compatibility contract was corrected to say so.
2. **The frontends compile against the agent crate.** Both do
   `use dvadva_agent::wire::protocol::WIRE_PROTOCOL_VERSION`. Inside one tree
   a mismatch is therefore impossible; across a deployment it is invisible.
   The deployment that matters is the one in use: a locally built GUI against
   a separately built agent on the VPS.
3. **Crate versions carry no information.** All seven crates are
   `version.workspace = true` — 1.8.0 — so a binary's version says nothing
   about what wire it speaks.
4. **The bridge frame protocol has no version field.** `remote/dvadva-bridge/src/proto.rs`
   frames (`Spawn`, `ListSessions`, `Version`) are unversioned; `Request::Version`
   exists, but `bridge::probe` (`wire-client/src/bridge.rs:68`) uses the answer
   as a liveness ping and a display string, never as a compatibility gate.
   This is the one hop where a mismatch can already happen today.
5. **`WIRE_PROTOCOL_LEGACY_VERSION` ("1.1") is not a negotiation input.** It is
   only used to tag wire files read back from disk (`wire/file.rs`).

### What shipped

- `wire/protocol.rs` gained `ProtocolVersion` (`parse` / `speaks` / `has`),
  `VersionError`, and `check_peer` — the one comparison both ends call.
  `ProtocolVersion::CURRENT` and `WIRE_PROTOCOL_VERSION` are pinned to each
  other by a test, so the struct and the string cannot drift.
- **Agent side**: `handle_initialize` gates on `params.protocol_version`
  before mutating anything (a refused client must not get its external tools
  registered on the way out) and answers the new
  `error_codes::PROTOCOL_VERSION_MISMATCH` (-32004). Malformed and
  incompatible are distinct messages: a frontend pointed at something that is
  not an agent should not be told its protocol is too old.
- **Client side**: `wire_client::check_server_protocol` reads the version out
  of the initialize result; both frontends fail the session with its message
  instead of proceeding into shapes they may not understand.
- **Bridge frames**: `BRIDGE_PROTOCOL_VERSION` ("1.0") beside the `BRIDGE1`
  magic, whose digit is the major and was already a hard gate — it now
  *reports* itself as one, so a `BRIDGE2` peer reads as a version mismatch and
  not as "not a bridge frame". The `version` reply carries the minor in a new
  `proto` field; a daemon built before it decodes fine and its silence means
  1.0.
- **Both numbers, everywhere a binary identifies itself**:
  `dvadva-agent --version` and `info`, the bridge's startup banner, and the
  `version` probe behind the GUI's connection light (`1.8.0 (frame 1.0)`).
- **Tests**: the version matrix (higher minor, lower minor, both foreign
  majors, malformed) as unit tests on `check_peer`; frame round trips
  including a pre-`proto` reply; the foreign-magic message on both halves; the
  two crates' framing constants pinned to each other; the bridge e2e version
  test extended. Verified against a live agent over stdio: `1.2`, `1.9` and
  `1.0` accepted, `2.0` and `banana` refused with their own messages.

Deliberately **not** done: splitting the frontends off the agent crate's
constant. They still `use dvadva_agent::wire::protocol`, so a same-tree build
cannot mismatch — which is fine, because the check exists for the split
deployment (local GUI, remote agent), and there it works. Extracting a shared
`wire-protocol` crate stays the deferred refactor `AGENTS.md` already names.

Next version number spent: **protocol 1.3**, by Phase 1.

---

## Phase 1 — many clients on one live agent — **DONE** (protocol 1.3)

Still connection-scoped lifetime; worth doing on its own merits (two GUIs, or
a GUI and the TUI, on one session). This is where the design risk was, and it
is nearly all inside `server/dvadva-agent/src/wire/`.

### What already helps

- `utils/broadcast.rs` has a `BroadcastQueue<T>` with subscribe/unsubscribe.
  Nothing on the wire path uses it yet — `WireServer::write_queue` is a single
  `Queue<Value>` drained by one writer task — but the primitive is there.
- The agent already emits `WireMessage::ApprovalResponse` after every
  resolution (`soul/kimisoul.rs:1432`), and `transcript.rs:305` already folds
  it back into the matching `Block::Approval`. The "dismiss the other client's
  dialog" event exists and is half-handled.

### Work as planned

**Fan-out and routing.** `write_queue` becomes a `BroadcastQueue` with a
per-connection `Queue` and a writer task per connection. Careful: not
everything is a broadcast. Events and reverse-RPC requests go to everyone;
**responses to a client's own `prompt` / `replay` / `cancel` must go back to
that connection only**. JSON-RPC ids are currently generated by a single
client (`WireClient::next_id`) and are only unique per connection, so the
server needs a `(connection, id)` key, or connection-scoped id rewriting, for
both directions.

**Gapless join.** Replay reads the file while the live stream also flows, so a
mid-turn attach can miss appends or double-show them. Stamp a monotonic `seq`
on each record as it is appended; attach subscribes to the broadcast *first*,
notes the seq, replays up to it, drops the overlap. Add `seq` to
`WireMessageRecord` as an optional field with a serde default so old
`wire.jsonl` files stay readable and new ones stay readable by old code.

**Per-connection state instead of a process-wide gate.** `initialize`
(`server.rs:224`), `prompt` (`:320`) and `replay` (`:473`) all return
`INVALID_STATE` while a turn is in progress. For `replay` that is precisely
the case attach needs. Split: one turn at a time stays a session-wide rule
(the `cancel_token` slot is correct); initialize and replay become
per-connection.

**Approval arbitration.** The server is already first-response-wins by
accident: the loser's response finds nothing in `pending.remove(&id)` and is
dropped with an `error!` log (`server.rs:~640`). Make that a defined outcome
with a quiet log, not an error. Client side, `inkvizitor` drives its modal
from `self.approvals` (`session.rs:208`), *not* from the transcript block, so
it must drop an entry when an `ApprovalResponse` event arrives for a request
it did not answer itself. `dvadva-tui` needs the same check.

**Re-arm open requests on replay.** `server.rs:504` says replayed requests are
read-only and deliberately does not register them as pending — correct today,
a bug the moment attach is real: a client attaching to an agent parked on an
approval sees the dialog and cannot answer it. Replay must know which pending
requests are still open and hand them over live.

**External tools are per-connection.** `initialize` registers a client's
external tools into the shared toolset. A second client collides on names; a
detaching client leaves tools registered that nothing can service, so the next
turn that calls one hangs until the wire closes. First cut: only the first
attachment may register external tools; later, ownership plus deregistration
on detach.

### What shipped, and where the plan was wrong

**The transport generalized itself.** `serve_connection(reader, writer)` is
the unit of "one attached client"; `serve()` over stdio is one caller and the
tests are another (a `tokio::io::duplex` pair per client). Phase 2's listener
is now a third caller rather than a rewrite. `WireServer` split into
`SessionCore` (soul, fan-out, open requests, the turn, tool ownership) and
`Connection` (initialized, catching up, its own replay token).

**Routing did not need `(connection, id)` keys after all.** The plan worried
about it because ids from `WireClient::next_id` are unique only per
connection. But nothing ever needs to route *by* a client's id: the handler
answering a call already knows whose call it was, so it unicasts. And every
reverse-RPC id is minted by the *agent* (a uuid), so `pending` stays one flat
map. Two clients both calling their first request `"1"` is a test, not a
problem — `two_clients_initialize_on_one_agent_without_their_ids_colliding`.

`Fanout` is a keyed map rather than the `BroadcastQueue` the plan named,
because the same registry has to serve both the broadcast and the unicast. It
prunes a client whose queue has gone rather than reporting an error: one
frontend closing its window must not silence the others.

**Gapless join: the `seq` field would not have worked, and is not needed.**
The plan assumed the file and the live stream carry the same messages. They
do not. `WireRecorder` subscribes to the **merged** queue (`channel.rs`) while
the wire server subscribes to the **raw** one, so the file holds coalesced
content parts and the live feed holds the deltas. A sequence number could not
have matched one against the other. What is real, and what shipped, is that
replay output and live output must not *interleave*: a catch-up stages the
connection's live traffic and releases it when the file walk ends.

What remains is inherent rather than a bug: a client attaching while an
assistant message is streaming sees only its tail, because the in-flight
message is not in the file yet and its beginning has already gone past. The
next turn's replay shows it whole. Closing that would mean unifying the raw
and merged streams, which is a `channel.rs` question, not an attach one.

**Approval arbitration** is now a defined first-answer-wins: the losers find
nothing in `pending` and are dropped with a debug line naming the race. Both
frontends drop their own dialog on the broadcast `ApprovalResponse` event, so
a modal that no longer decides anything comes down.

**Re-arming open requests** had an ordering trap the plan did not name. Both
frontends render a request that arrives while they are replaying and
deliberately do *not* arm it (`session.rs`, `agent.rs`: "historical: already
answered"). So the still-open requests have to be handed over *after* the
replay response, not before, or a live approval is filed as history. Caught by
`a_client_can_attach_to_a_parked_agent_and_answer_the_approval`, which is the
end-to-end test of the whole phase: attach to a parked agent, replay, answer
the approval the first client raised, watch the first client's dialog vanish.

**External tools** went straight to the "later" design — ownership by
`ConnId` plus `KimiToolset::unregister_external_tool` on detach — because the
first-attachment-only rule needed the same bookkeeping and would still have
left dead registrations behind.

**Protocol 1.3** spends the minor on one additive field: `capabilities` in the
`initialize` result (`{"multi_client": true}`). Everything else in this phase
is a relaxation or a fix, invisible to a 1.2 client. The field exists so a
frontend or supervisor can *ask* rather than infer capability from a number,
which is what Phase 2 will need.

**Also found, not fixed**: `cancel` during a replay was unreachable before
this phase, because the replay ran inline and blocked the connection's read
loop. Replay is now its own task and its own per-connection token, so a client
can abort a long catch-up; a `cancel` with no replay in flight still stops the
turn, as before.

---

## Phase 2 — detach — **DONE** (frame protocol 1.1; wire protocol still 1.3)

Small once Phase 1 lands.

Today detach means kill: `serve()` is hardwired to `tokio::io::stdin()`
(`server.rs:55-56`) and exits on EOF (`:117`); the bridge spawns one agent per
TCP connection with `kill_on_drop(true)` (`remote_daemon.rs:180`) and
documents connection lifetime *as* agent lifetime.

- ~~**Generalize the transport.**~~ **Done.** `serve()` over a listener rather
  than stdio; loopback TCP, since the bridge already assumes TCP and it
  behaves the same on both platforms. Keep stdio mode — it is the one-shot
  path and every test uses it.
- ~~**stdin EOF stops being fatal**~~ **Done**, on the listening transport;
  the last client detaching leaves the process running.
- ~~**Make `dvadva-bridge remote` a supervisor.**~~ **Done.** `Request::Spawn { args }`
  becomes `Request::Attach { session_id }`: find-or-start an agent for that
  session, relay, and do not kill it when the socket drops. The daemon already
  owns session listing (`list_sessions`), the frontends already know how to
  talk to it, and `--remote 127.0.0.1:9000` against a local daemon then *is*
  the headless story on the dev box too. Direct-spawn stays for one-shot use.
  This also settles Windows: there is no `setsid`, so somebody must spawn with
  `DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP`, and the daemon is the natural
  somebody.
- ~~**A live-session registry**~~ **Done**: pid + endpoint under `~/.kimi`,
  with reaping of entries whose pid is gone. `list_sessions` marks which
  sessions are live.
- ~~**Drop `kill_on_drop`**~~ **Done** for supervised agents, and rework the
  exit-trailer machinery (`remote_daemon.rs`), which assumes the daemon owns
  the child's stderr for the life of the connection.

### Constraint: one process per session

Do not let one process host several souls. `app.rs:196` does a process-wide
`kaos::chdir` per run, and that is the most visible of the process globals, not
the only one. Multiplexing is the supervisor's job. Everything above is
written on that assumption.

### Security

A listening agent is a remote shell with no auth — the same hazard
`remote/PLAN.md` handles by insisting on loopback plus `ssh -L`. Same rule
applies, written down again at the new listener, plus a token file so a second
local user cannot attach to your session.

### What shipped in the soft-detach slice

`wire/listener.rs`, `--listen [ADDR]`, `--listen-token-file PATH`, and the
tests in `tests/wire_detach.rs` (over real sockets, because half the point is
what a *socket* does when it drops).

**Lifetime turned out not to be a transport property that needed
generalizing — it was a policy that had never been named.** `serve()` over
stdio still ends the session at EOF, deliberately: that is the one-shot path,
where the frontend spawned the process and closing stdin is how it says it is
done. The listener simply does not call `shutdown` when a connection ends.
Nothing below `serve_connection` changed, which is the Phase 1 seam paying
off.

**`--listen` is additive, not a mode.** The agent keeps serving whoever handed
it its pipes *and* the socket, so a bridge-spawned agent can outlive its
spawner without the bridge changing first. It skips stdio when stdin is a
terminal, or a human running the command would get wire JSON sprayed at them.

**The token is transport, not protocol, and that mattered.** Putting it in
`initialize` params would have been the additive-protocol answer and would
have been wrong: `initialize` is not a gate, so nothing stops an unauthorised
client from sending `prompt` first. It is a one-line handshake read before any
wire byte, which also means the protocol stays at 1.3 — this slice spends no
version number. The handshake reader takes exactly one line and leaves the
rest buffered, so a client may pipeline its `initialize` behind it.

**The bound address is announced on stderr** as `dvadva-agent: listening
{json}` (addr, session, pid, protocol, token file). A supervisor spawning with
port 0 has no other way to learn the port: logs go to a file, and stdout may be
a client's wire.

**Also found and fixed**: the reader's buffer was `100 * 1024 * 1024`, one
allocation per connection. Harmless when the only connection was stdio, not
once every attached client makes one. Line length never depended on it —
`read_until` grows its own output — so it is now 64 KB.

### What shipped in the supervisor slice

`live.rs` (the registry), `Request::Attach` and the supervising half of
`remote_daemon.rs`, plus the `--listen` mock agent the e2e suite needed in
order to test any of it honestly.

**Liveness is the endpoint, not the pid.** The plan said "reaping of entries
whose pid is gone". What a reader actually wants to know is whether it can
still attach, and a live pid with a dead listener answers the wrong question
— while asking the pid portably costs either a new dependency or `unsafe`,
and the workspace denies the second. So `Registry::list` connects, keeps what
answers, and deletes what does not; the pid stays in the entry for the humans
(`kill`, a task manager, a log line). One consequence worth knowing: a listing
opens a connection to every live agent, so the listener logs that shape of
close at debug rather than warn (`SilentClose`) — otherwise every listing left
a warning behind in every agent.

**The supervisor waits by pid, not by session id.** The interesting case is a
brand-new session, whose id nobody knows until the agent mints it. So the
daemon starts the agent, watches the registry for *its* pid, and reports the
id it finds in the ack (`Reply::session`) — which is the only way a caller who
asked for a new session learns which one it got. It never has to parse the
announce line, which stays what it is: the channel for whoever holds the
pipes.

**Three ways a supervised agent is not the daemon's child in spirit**:
`kill_on_drop(false)`, its own process group (`process_group(0)` /
`DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP` — this is the Windows answer to
there being no `setsid`), and **stderr to a file** rather than to a pipe. That
last one was not in the plan and is not cosmetic: a piped stderr dies with the
daemon, and the agent writes to stderr from places that *panic* if the write
fails (`soul/toolset.rs` reports MCP connections that way). An agent that
cannot survive its supervisor is not detached.

**The exit trailer moved rather than being reworked.** On the supervised path
its case — a bad work directory, a missing key — happens *before* any relay
exists, so it is the ack that carries the diagnosis, quoted from the agent's
log. During a relay the trailer is still there, for the agent that falls over
under an attached client. What it cannot do is diagnose an agent this
connection did not start: only the daemon that spawned one knows its log file
by name, and inventing a claim there would be worse than the plain fact.

**Telling a detach from a death is an ordering question, and getting it wrong
was the one real bug this slice had.** The first cut asked whether the
client-to-agent copy ended *cleanly*, on the theory that an EOF is somebody
leaving and an error is the agent going away underneath. It is not: a killed
frontend with bytes still in its receive buffer resets the connection, so a
hard drop — the exact case the phase exists for — arrived as an error and got
filed as a dead agent. What actually distinguishes them is which direction
ended first, since a client leaving *causes* the agent to close its side a
moment later while an agent dying leaves the other copy pending on a client
that is still there. Caught by running the real binaries, not by the tests,
which is its own lesson; there is a test for it now
(`a_client_that_says_goodbye_gets_no_trailer`), and both endings now
half-close the client socket explicitly rather than letting it fall out of
scope.

**`--session` is the caller's to write.** The daemon uses the header's session
id as a registry key and nothing else — it does not synthesize agent argv from
it. A cold resume therefore names the session twice, once for each meaning,
which is what the frontends already do on the `spawn` path.

**Frame protocol 1.1**, additively: the `attach` op, `Reply::session`, and
`SessionEntry::live`. A 1.0 daemon refuses an `attach` frame as an unknown op,
which is exactly why the minor is reported in the `version` reply. The wire
protocol is untouched at 1.3 — this whole phase happens beside it.

**Also shipped**: `list_sessions` marks the live ones, *and* appends live
sessions the cold listing has no file for yet (a brand-new session has no
context to read, and `Session::list` skips it — a live session nobody can see
is a live session nobody can attach to). The local listing
(`wire_client::session_list`) reads the same registry, for the same reason.

**Still open, and Phase 3's**: an explicit stop over the wire. Stopping a
detached agent today is `SIGTERM`/interrupt (handled), a kill, or the
supervised path's `stop` semantics that do not exist yet. Windows note: there
is no graceful terminate signal, so `taskkill` is a hard kill there — which
the registry survives, because a stale entry is reaped by whoever reads it.

---

## Phase 3 — policy and frontends

The long tail, and where "one agent, many attached clients" either feels good
or feels haunted.

- **Reconnect.** Both frontends treat `AgentExited` as terminal
  (`session.rs:257`, `agent.rs:124`). Detach is not death and needs its own
  state, plus a way back in.
- **Headless approvals.** A turn that hits an approval with nobody attached
  blocks forever. Options: `--yolo`, a timeout that rejects, or park-and-surface
  on next attach (best, and cheap given the Phase 1 re-arm work).
- **Idle shutdown** and an explicit stop, so detached agents do not accumulate.
- **The resume list** distinguishes live sessions from cold ones, in the GUI
  palette and the TUI menu.

---

## Order of dependence

```
Phase 0 (versioning)  ──▶ Phase 1 (fan-out, 1.3)  ──▶ Phase 2 (detach)  ──▶ Phase 3
   done                       done                  └──▶ registry / supervisor
                                                          done (frame 1.1)
```

Phase 1 is shippable alone. Phase 2 without Phase 1 would give a detached agent
that only one client at a time can reach, which is most of the cost for a
fraction of the value.
