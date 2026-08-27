# Wire Module Notes

## Scope

- `types.rs`: wire message structs/enums + `WireMessageEnvelope`.
- `serde.rs`: JSON (de)serialization helpers.
- `file.rs`: `WireFile` JSONL persistence, metadata header, `WireMessageRecord`.
- `protocol.rs`: the protocol version, and the compatibility rule for it
  (`ProtocolVersion`, `check_peer`).
- `jsonrpc.rs`: JSON-RPC models/utilities for wire server.
- `server.rs`: the JSON-RPC wire server. `SessionCore` is what the session
  owns, `Connection` is what one attached client owns; `serve_connection`
  serves one client over any reader/writer pair, and stdio is just its first
  caller.
- `fanout.rs`: routing to the attached clients (`Fanout`, `ConnId`).
- `listener.rs`: the detachable transport. A loopback socket, its token
  handshake, and the lifetime rule that a client leaving is not the end of
  the session.
- `channel.rs`: `Wire`, `WireSoulSide`, `WireUISide`, merge + recording logic.

## Compatibility Rules

- `major.minor`: a major bump is breaking, a minor bump is additive only. Both
  ends call `check_peer` during `initialize` and refuse a foreign major; the
  agent answers `error_codes::PROTOCOL_VERSION_MISMATCH`, the frontends fail
  the session (`wire_client::check_server_protocol`). Gate any message
  introduced in a later minor on `ProtocolVersion::has`.
- `ProtocolVersion::CURRENT` and `WIRE_PROTOCOL_VERSION` are two spellings of
  one fact; a test pins them together. Bump both.

- Envelope `type` strings must match the original Python class names (stability invariant; wire clients depend on them).
- `ContentPart` wire messages always use `type="ContentPart"` at the envelope layer.
- `ApprovalRequestResolved` must map to `ApprovalResponse` for backward compatibility.
- `SubagentEvent.event` is serialized as an embedded `WireMessageEnvelope`.

## Many Clients, One Agent

- **Events and reverse-RPC requests broadcast; responses are unicast.** A
  response id was minted by one client's `next_id` and is unique only within
  that connection, so it goes back to the asker alone. Nothing ever routes
  *by* a client-minted id — the handler that answers already knows whose
  question it was. Reverse-RPC ids are minted by the agent and are globally
  unique, which is why `SessionCore::pending` is one flat map.
- **Session-wide vs. per-connection.** One turn at a time is a session rule
  (`cancel_token`). `initialize` and `replay` are per-connection: refusing
  them mid-turn would refuse exactly the case worth attaching to.
- **A catch-up is staged.** `replay` starts by snapshotting the open requests
  and switching the connection to staging in one step, so nothing is both
  replayed from the file and delivered live. The staged traffic is released
  when the file walk ends. `request_approval` publishes while still holding
  the `pending` lock so that snapshot cannot race a new request.
- **Open requests are re-armed *after* the replay response.** Both frontends
  render a request that arrives while they are replaying and deliberately do
  not arm it, so handing them over any earlier files a live approval as
  history.
- **First answer wins.** With several clients the same dialog is on every
  screen; the losers find nothing in `pending` and are dropped with a debug
  line. Clients drop their own dialog on the `ApprovalResponse` event.
- **External tool names are owned by the client that registered them**
  (`SessionCore::tool_owner`) and are unregistered when it detaches — a
  registration nobody can service would hang the next turn that called it.
- **Not gapless mid-stream.** A client attaching while an assistant message
  is streaming sees only its tail: the file records the *merged* stream and
  the live feed is the *raw* one, so the in-flight message is in neither the
  replay nor, from the start, the live traffic. Inherent to the two
  representations, not a sequencing bug; the next turn's replay shows it
  whole.

## Detach: Two Transports, Two Lifetimes

- **The protocol does not know which transport it is on.** `serve_connection`
  is the same on both; what differs is only what the end of a connection
  means. Over stdio it ends the process (`serve` calls `shutdown`), because
  the frontend spawned us and closing stdin is how it says it is done. Over
  the listener it ends nothing.
- **The token is transport, not protocol.** It is checked before the first
  wire byte, because `initialize` is not a gate — nothing stops a client from
  sending `prompt` first. Stdio does not carry it: inheriting the pipes is a
  stronger claim than knowing a secret, and requiring it there would have
  broken every existing caller.
- **Loopback only, refused at the bind.** An agent that takes a `prompt` runs
  shell commands for whoever reaches it. Crossing machines is `ssh -L`'s job,
  as in `remote/PLAN.md`.
- **The handshake must not eat what follows it.** A client may pipeline its
  `initialize` in the same write, so the reader takes exactly one line and
  leaves the buffer to the wire server.
- **The announce line on stderr is an interface.** `dvadva-agent: listening
  {json}` carries addr, session, pid and token file; a supervisor spawning
  with port 0 has no other way to learn the port. Logs go to a file and
  stdout may be a client's wire, so stderr is the only channel left.
- **Read buffers are per connection.** The reader's capacity used to be
  100 MB, which was one allocation when the only client was stdio. Line
  length never depended on it (`read_until` grows its own output), so it is
  now 64 KB.
- **Two channels say where the agent is, for two askers.** The announce line
  answers the process that spawned it and holds its pipes. The live-session
  registry (`crate::live`, `~/.kimi/live/<session>.json`) answers everyone
  who did not: a second frontend, a supervisor that has since restarted, a
  person with a terminal. A listing is enough to attach with — it names the
  address *and* the token file — which is what `dvadva-bridge`'s `attach`
  op does.
- **A connection that closes without saying anything is a probe, not an
  intrusion.** The registry decides liveness by connecting, so that shape of
  failure logs at debug (`SilentClose`); everything else still warns. A
  listing must not fill an agent's log.

## Merge Behavior

- `WireSoulSide` merges adjacent `ContentPart`, `ToolCall`, and `ToolCallPart` via `merge_in_place`.
- `flush()` emits the current merge buffer before non-mergeable events.

## Subscription Notes

- `Wire` pre-subscribes a default UI queue so early events are buffered before the UI loop starts.
- `Wire::join()` must be awaited after `Wire::shutdown()` to flush recorder writes.
