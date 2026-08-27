package dev.dvadva.android.proto

import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.jsonPrimitive

/**
 * The wire protocol's version and the rule for comparing two of them — a
 * Kotlin port of `server/dvadva-agent/src/wire/protocol.rs`.
 *
 * Two different numbers travel with every connection and must not be
 * confused: the *protocol version* here, which says what the two ends may say
 * to each other, and the component version, which says which build you are
 * talking to. Only the first decides compatibility.
 *
 * The rule: `major.minor`. A major bump is breaking — refuse the peer. A
 * minor bump is additive only, so a peer's *higher* minor is safe to talk to
 * (ignore what you do not recognize) and a peer's *lower* minor means do not
 * use what it predates.
 */
object Protocol {
    /** The protocol this build speaks, as it goes on the wire. */
    const val WIRE_PROTOCOL_VERSION = "1.3"

    /** [WIRE_PROTOCOL_VERSION] as a pair; the two spellings must never drift (pinned by a test). */
    val CURRENT = ProtocolVersion(1, 3)

    /** Parse exactly `major.minor`. Deliberately strict: this is a gate. */
    fun parseVersion(text: String): ProtocolVersion {
        val parts = text.split('.')
        if (parts.size != 2) throw VersionException.malformed(text)
        val major = parts[0].toUIntOrNull() ?: throw VersionException.malformed(text)
        val minor = parts[1].toUIntOrNull() ?: throw VersionException.malformed(text)
        return ProtocolVersion(major.toInt(), minor.toInt())
    }

    /** Check a peer's declared version against this build's, handing back the parsed peer version. */
    fun checkPeer(declared: String): ProtocolVersion {
        val peer = parseVersion(declared)
        if (peer.major != CURRENT.major) {
            throw VersionException(
                "wire protocol $declared is not compatible with this build's " +
                    "$WIRE_PROTOCOL_VERSION: major versions must match",
            )
        }
        return peer
    }

    /**
     * Check the protocol version an agent declared in its `initialize` result.
     * The agent runs the same check on *our* declared version from its side.
     */
    fun checkServerProtocol(result: JsonObject): ProtocolVersion {
        val declared = result["protocol_version"]?.jsonPrimitive?.contentOrNull
        if (declared == null) {
            throw VersionException(
                "the agent's initialize result names no protocol version: it predates " +
                    "version negotiation, or it is not a dvadva-agent",
            )
        }
        return try {
            checkPeer(declared)
        } catch (e: VersionException) {
            if (e.message?.contains("names no protocol version") == true) throw e
            throw VersionException("${e.message} (the two binaries need to match)")
        }
    }
}

/** A parsed `major.minor` protocol version. */
data class ProtocolVersion(val major: Int, val minor: Int) {
    /** Whether a feature introduced in [minor] may be used with this peer. */
    fun has(minor: Int): Boolean = this.minor >= minor

    override fun toString(): String = "$major.$minor"
}

class VersionException(message: String) : Exception(message) {
    companion object {
        fun malformed(text: String) =
            VersionException("malformed wire protocol version \"$text\" (expected `major.minor`)")
    }
}
