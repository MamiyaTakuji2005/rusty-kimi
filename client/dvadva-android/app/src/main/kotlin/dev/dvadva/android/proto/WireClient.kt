package dev.dvadva.android.proto

import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.receiveAsFlow
import kotlinx.coroutines.launch
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.coroutines.withContext
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.put
import java.io.BufferedReader
import java.io.BufferedWriter
import java.io.IOException
import java.io.InputStreamReader
import java.io.OutputStreamWriter
import java.net.Socket
import java.util.concurrent.atomic.AtomicLong

/**
 * The wire-protocol client over a `dvadva-bridge` TCP connection — the Kotlin
 * twin of `WireClient::connect_tcp` in `client/wire-client/src/lib.rs`.
 *
 * One TCP connection per session: BRIDGE1 spawn handshake, then
 * newline-delimited JSON-RPC 2.0 both ways until either side closes. The
 * Rust client spawns a reader thread and pokes its UI awake through a `wake`
 * hook; here the reader coroutine feeds a [Channel] and Compose collects the
 * [inbound] flow, so no wake hook is needed.
 *
 * Shutdown half-closes the write side (the daemon turns it into the remote
 * agent's stdin EOF — the graceful "please exit"). Phase 2 of
 * `PLAN-detached-agent.md` will let the agent outlive this socket; this class
 * deliberately knows nothing about that yet, so the change lands in the
 * session layer, not the transport.
 */
class WireClient private constructor(
    private val socket: Socket,
    private val reader: BufferedReader,
    private val writer: BufferedWriter,
    private val writeMutex: Mutex,
    private val inboundChannel: Channel<Inbound>,
) {
    private val nextId = AtomicLong(1)

    /** Everything the agent sends, in arrival order. Completes when the stream ends. */
    val inbound: Flow<Inbound> = inboundChannel.receiveAsFlow()

    /** Send a JSON-RPC request; returns the generated id (`client-N`, unique within this connection). */
    suspend fun sendRequest(method: String, params: JsonObject): String {
        val id = "client-${nextId.getAndIncrement()}"
        sendRaw(
            buildJsonObject {
                put("jsonrpc", "2.0")
                put("id", id)
                put("method", method)
                put("params", params)
            },
        )
        return id
    }

    /** Answer a reverse-RPC request from the agent with a success result. */
    suspend fun respondResult(id: String, result: JsonObject) {
        sendRaw(
            buildJsonObject {
                put("jsonrpc", "2.0")
                put("id", id)
                put("result", result)
            },
        )
    }

    /** Answer a reverse-RPC request from the agent with an error. */
    suspend fun respondError(id: String, code: Int, message: String) {
        sendRaw(
            buildJsonObject {
                put("jsonrpc", "2.0")
                put("id", id)
                put("error", buildJsonObject {
                    put("code", code)
                    put("message", message)
                })
            },
        )
    }

    private suspend fun sendRaw(value: JsonObject) {
        withContext(Dispatchers.IO) {
            writeMutex.withLock {
                writer.write(value.toString())
                writer.write("\n")
                writer.flush()
            }
        }
    }

    /**
     * Ask the peer to exit: half-close the write side. The remote agent sees
     * stdin EOF and exits on its own; its final output and the daemon's exit
     * trailer still arrive on [inbound] afterwards.
     */
    suspend fun shutdown() {
        withContext(Dispatchers.IO) {
            try { socket.shutdownOutput() } catch (_: IOException) {}
        }
    }

    /** Tear the whole connection down without waiting for a graceful exit. */
    fun close() {
        try { socket.close() } catch (_: IOException) {}
        inboundChannel.close()
    }

    companion object {
        /**
         * Connect to a `dvadva-bridge` daemon at `endpoint` (`host:port`),
         * have it spawn an agent with [agentArgs], then speak the wire
         * protocol over the resulting byte stream.
         *
         * The bridge handshake happens here, fully: a refused spawn
         * (unreachable daemon, missing agent binary) surfaces as a
         * [BridgeException] instead of a confusing protocol error once the
         * session is already running. Every step of it is bounded — a daemon
         * that accepts but never answers must not freeze the UI.
         */
        suspend fun connectTcp(
            endpoint: String,
            agentArgs: List<String>,
            scope: CoroutineScope,
        ): WireClient {
            val socket = Bridge.connect(endpoint, Bridge.CONNECT_TIMEOUT_MS)
            try {
                val writer = BufferedWriter(OutputStreamWriter(socket.getOutputStream(), Charsets.UTF_8))
                writer.write(Bridge.spawnHeader(agentArgs))
                writer.write("\n")
                writer.flush()

                // Exactly one reply frame before the relay starts. The read
                // timeout comes off again once the handshake is done — the
                // wire stream itself is idle for as long as the user thinks.
                socket.soTimeout = Bridge.HANDSHAKE_TIMEOUT_MS
                val reader = BufferedReader(InputStreamReader(socket.getInputStream(), Charsets.UTF_8))
                val ack = try {
                    Bridge.readFrameLine(reader)
                } catch (e: BridgeException) {
                    throw BridgeException("bridge `$endpoint` handshake failed: ${e.message}")
                }
                val reply = try {
                    Bridge.decodeReply(ack)
                } catch (e: BridgeException) {
                    throw BridgeException("bad bridge handshake: ${e.message}")
                }
                if (!reply.ok) {
                    throw BridgeException(
                        "bridge `$endpoint` refused spawn: ${reply.error ?: "bridge refused spawn"}",
                    )
                }
                socket.soTimeout = 0

                // The ack read may have buffered early agent output; the same
                // BufferedReader becomes the stream reader so nothing is lost.
                val inboundChannel = Channel<Inbound>(Channel.UNLIMITED)
                val client = WireClient(
                    socket = socket,
                    reader = reader,
                    writer = writer,
                    writeMutex = Mutex(),
                    inboundChannel = inboundChannel,
                )
                client.startReader(scope)
                return client
            } catch (e: Exception) {
                try { socket.close() } catch (_: IOException) {}
                throw e
            }
        }
    }

    /**
     * Drain the stream until it ends. Wire lines are NOT bounded at 64 KiB —
     * only bridge frames are — so this loop uses plain `readLine`. The
     * daemon's exit trailer (the one non-wire line this transport ever
     * produces) is turned into [Inbound.AgentExited].
     */
    private fun startReader(scope: CoroutineScope) {
        scope.launch(Dispatchers.IO) {
            try {
                while (true) {
                    val line = reader.readLine()
                    if (line == null) {
                        inboundChannel.trySend(Inbound.AgentExited("remote connection closed"))
                        break
                    }
                    val trimmed = line.trim()
                    if (trimmed.isEmpty()) continue
                    val trailer = Bridge.exitTrailer(trimmed)
                    if (trailer != null) {
                        inboundChannel.trySend(Inbound.AgentExited(trailer))
                        break
                    }
                    inboundChannel.trySend(classifyLine(trimmed))
                }
            } catch (e: IOException) {
                inboundChannel.trySend(Inbound.AgentExited("read error: ${e.message}"))
            } finally {
                inboundChannel.close()
            }
        }
    }
}
