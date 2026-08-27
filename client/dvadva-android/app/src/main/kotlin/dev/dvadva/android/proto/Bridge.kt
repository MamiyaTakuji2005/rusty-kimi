package dev.dvadva.android.proto

import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonArray
import kotlinx.serialization.json.JsonNull
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive
import kotlinx.serialization.json.put
import kotlinx.serialization.json.putJsonArray
import java.io.BufferedReader
import java.io.IOException
import java.io.InputStreamReader
import java.io.OutputStreamWriter
import java.io.BufferedWriter
import java.net.InetAddress
import java.net.InetSocketAddress
import java.net.Socket
import java.net.SocketTimeoutException

/**
 * The client side of the dvadva-bridge control framing — a Kotlin port of
 * `client/wire-client/src/bridge.rs`.
 *
 * A remote connection begins with one `BRIDGE1` frame: the client states what
 * it wants — spawn an agent with these args, or list the sessions on the far
 * side — the daemon answers with one reply frame, and everything after that is
 * the opaque dvadva-agent wire stream.
 *
 * Constants, frame shapes and error strings are deliberately byte-identical to
 * the Rust twin; `BridgeGoldenTest` pins the important ones against the same
 * vectors the Rust tests assert. Change both sides or neither.
 */
object Bridge {
    /** Magic prefix of every bridge frame — kept in sync with the daemon side. */
    const val MAGIC = "BRIDGE1"

    /** The magic without its version digit, for telling "a bridge frame from a build we cannot talk to" apart from "not a bridge frame at all". */
    const val MAGIC_FAMILY = "BRIDGE"

    /** This build's frame protocol version, `major.minor`, matching `dvadva_bridge::proto::BRIDGE_PROTOCOL_VERSION`. */
    const val PROTOCOL_VERSION = "1.0"

    /** Upper bound on a frame line, matching the daemon side. A client pointed at the wrong port must fail, not buffer forever. */
    const val MAX_LINE_BYTES = 64 * 1024

    /** How long to wait for the TCP connection itself. */
    const val CONNECT_TIMEOUT_MS = 10_000

    /** How long to wait for the daemon's single reply frame. */
    const val HANDSHAKE_TIMEOUT_MS = 15_000

    private val json = Json

    /** The header asking a bridge daemon to spawn an agent with [args] (verbatim agent CLI arguments: `-w`, `--session`, …) and relay. */
    fun spawnHeader(args: List<String>): String = frame(
        buildJsonObject {
            put("op", "spawn")
            putJsonArray("args") { args.forEach { add(it) } }
        },
    )

    /** The header asking a bridge daemon for the sessions on its machine. */
    fun listSessionsHeader(): String = frame(
        buildJsonObject { put("op", "list_sessions") },
    )

    /** The header asking a bridge daemon whether it is there at all. */
    fun versionHeader(): String = frame(
        buildJsonObject { put("op", "version") },
    )

    /** Encode a request frame line (without the trailing newline). Key order matches the Rust tests byte-for-byte. */
    private fun frame(payload: JsonObject): String = "$MAGIC ${payload.toString()}"

    /** The daemon's single reply frame to any request. */
    data class Reply(
        val ok: Boolean,
        val error: String?,
        val sessions: List<SessionEntry>?,
        val version: String?,
        val proto: String?,
    )

    /** One entry of a `list_sessions` reply (the same shape as `wire_client::session_list::ResumeEntry`). */
    data class SessionEntry(
        val id: String,
        val title: String,
        val workDir: String,
        val updatedAt: Double,
    )

    /** Parse a reply frame line (as produced by the daemon). */
    fun decodeReply(line: String): Reply {
        if (!line.startsWith(MAGIC)) throw BridgeException(magicMismatch(line))
        val body = line.removePrefix(MAGIC).trimStart()
        val element = try {
            json.parseToJsonElement(body)
        } catch (e: Exception) {
            throw BridgeException("bad bridge reply: ${e.message}")
        }
        val obj = element as? JsonObject
            ?: throw BridgeException("bad bridge reply: not a JSON object")

        val ok = obj["ok"]?.jsonPrimitive?.booleanOrNull
            ?: throw BridgeException("bad bridge reply: missing ok")
        val sessions = try {
            (obj["sessions"] as? JsonArray)?.map { entry ->
                val e = entry.jsonObject
                SessionEntry(
                    id = e.strOrThrow("id"),
                    title = e.str("title") ?: "",
                    workDir = e.str("work_dir") ?: "",
                    updatedAt = e.str("updated_at")?.toDoubleOrNull() ?: 0.0,
                )
            }
        } catch (e: BridgeException) {
            throw e
        } catch (e: Exception) {
            throw BridgeException("bad bridge reply: ${e.message}")
        }
        return Reply(
            ok = ok,
            error = obj["error"]?.jsonPrimitive?.contentOrNull,
            sessions = sessions,
            version = obj["version"]?.jsonPrimitive?.contentOrNull,
            proto = obj["proto"]?.jsonPrimitive?.contentOrNull,
        )
    }

    /**
     * Why a line did not start with our magic. A reply from a *different*
     * bridge major is a version mismatch and has to say so: reporting it as
     * "not a bridge reply" would send whoever hit it looking for a networking
     * fault instead of for a stale daemon.
     */
    private fun magicMismatch(line: String): String {
        val word = line.trim().split(Regex("\\s+")).firstOrNull() ?: ""
        return if (word.startsWith(MAGIC_FAMILY) && word != MAGIC) {
            "bridge frame protocol `$word` is not compatible with this build's `$MAGIC`: the two binaries need to match"
        } else {
            "not a bridge reply (missing $MAGIC prefix)"
        }
    }

    /**
     * The daemon's exit trailer, if this relayed line is one. After the remote
     * agent's stdout ends, the daemon appends a final frame carrying the exit
     * status and the agent's stderr tail. Wire-protocol JSON never starts with
     * the magic, so the prefix is enough to tell the two apart.
     */
    fun exitTrailer(line: String): String? {
        if (!line.startsWith(MAGIC)) return null
        return try {
            val reply = decodeReply(line)
            reply.error?.takeIf { it.isNotEmpty() } ?: "remote agent exited"
        } catch (e: BridgeException) {
            "remote agent exited (unreadable bridge trailer: ${e.message})"
        }
    }

    /**
     * Read one `\n`-terminated frame line, bounded by [MAX_LINE_BYTES]. The
     * daemon-side twin is `dvadva_bridge::proto::read_line`: same cap, and
     * bytes the peer pipelined after the newline stay in the reader for
     * whoever owns the stream next (for the session, the agent's first wire
     * lines).
     */
    fun readFrameLine(reader: BufferedReader): String {
        val buf = StringBuilder()
        try {
            while (true) {
                val c = reader.read()
                if (c == -1) {
                    throw BridgeException(
                        if (buf.isEmpty()) {
                            "the daemon closed the connection without replying"
                        } else {
                            "the daemon closed the connection mid-frame"
                        },
                    )
                }
                if (c == '\n'.code) break
                buf.append(c.toChar())
                if (buf.length > MAX_LINE_BYTES) {
                    throw BridgeException("bridge frame exceeds size limit (is this a dvadva-bridge daemon?)")
                }
            }
        } catch (e: SocketTimeoutException) {
            throw BridgeException("the daemon did not answer within ${HANDSHAKE_TIMEOUT_MS / 1000}s")
        }
        return buf.toString().trimEnd()
    }

    /** Connect to `host:port` with a bounded wait, trying every address it resolves to. */
    suspend fun connect(endpoint: String, timeoutMs: Int = CONNECT_TIMEOUT_MS): Socket =
        withContext(Dispatchers.IO) {
            val colon = endpoint.lastIndexOf(':')
            if (colon <= 0 || colon == endpoint.length - 1) {
                throw BridgeException("failed to resolve bridge `$endpoint`: not a host:port")
            }
            val host = endpoint.substring(0, colon)
            val port = endpoint.substring(colon + 1).toIntOrNull()
                ?: throw BridgeException("failed to resolve bridge `$endpoint`: bad port")

            val addrs = try {
                InetAddress.getAllByName(host)
            } catch (e: Exception) {
                throw BridgeException("failed to resolve bridge `$endpoint`: ${e.message}")
            }
            var last: Exception? = null
            for (addr in addrs) {
                try {
                    val socket = Socket()
                    socket.tcpNoDelay = true
                    socket.connect(InetSocketAddress(addr, port), timeoutMs)
                    return@withContext socket
                } catch (e: Exception) {
                    last = e
                }
            }
            throw BridgeException(
                "failed to connect to bridge `$endpoint`: ${last?.message ?: "no address to try"}",
            )
        }

    /**
     * Ask the daemon at [endpoint] for its version — the liveness probe behind
     * a UI's connection indicator. Deliberately its own short-lived
     * connection: each session dials its own, and a probe that spawns nothing
     * is safe to run on a timer.
     */
    suspend fun probe(endpoint: String, timeoutMs: Int = CONNECT_TIMEOUT_MS): String =
        withContext(Dispatchers.IO) {
            val socket = connect(endpoint, timeoutMs)
            try {
                socket.soTimeout = timeoutMs
                val writer = BufferedWriter(OutputStreamWriter(socket.getOutputStream(), Charsets.UTF_8))
                writer.write(versionHeader())
                writer.write("\n")
                writer.flush()
                val reader = BufferedReader(InputStreamReader(socket.getInputStream(), Charsets.UTF_8))
                val reply = decodeReply(readFrameLine(reader))
                if (!reply.ok) {
                    throw BridgeException(reply.error ?: "bridge refused the probe")
                }
                // A daemon built before the `proto` field existed never names
                // one; that absence *is* the answer (frame 1.0), but say
                // "unstated" rather than print a number the daemon never sent.
                val version = reply.version ?: "unknown"
                val proto = reply.proto ?: "unstated"
                "$version (frame $proto)"
            } finally {
                try { socket.close() } catch (_: IOException) {}
            }
        }

    /** Ask the daemon at [endpoint] for the sessions living on its machine, newest first. */
    suspend fun listSessions(endpoint: String): List<SessionEntry> =
        withContext(Dispatchers.IO) {
            val socket = connect(endpoint)
            try {
                socket.soTimeout = HANDSHAKE_TIMEOUT_MS
                val writer = BufferedWriter(OutputStreamWriter(socket.getOutputStream(), Charsets.UTF_8))
                writer.write(listSessionsHeader())
                writer.write("\n")
                writer.flush()
                val reader = BufferedReader(InputStreamReader(socket.getInputStream(), Charsets.UTF_8))
                val reply = decodeReply(readFrameLine(reader))
                if (!reply.ok) {
                    throw BridgeException(reply.error ?: "bridge listing failed")
                }
                reply.sessions.orEmpty().sortedByDescending { it.updatedAt }
            } finally {
                try { socket.close() } catch (_: IOException) {}
            }
        }
}

/** A bridge protocol failure, carrying the same message the Rust frontends surface. */
class BridgeException(message: String) : Exception(message)

/** Lenient field getters: a missing or null field is a bad frame, not a crash. */
internal fun JsonObject.str(key: String): String? =
    (this[key] as? JsonPrimitive)?.takeIf { it !is JsonNull }?.content

internal fun JsonObject.strOrThrow(key: String): String =
    str(key) ?: throw BridgeException("bad bridge reply: missing `$key`")
