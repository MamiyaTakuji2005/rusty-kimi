package dev.dvadva.android.proto

import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.put
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Test

/**
 * Mirrors the version-gate tests in `server/dvadva-agent/src/wire/protocol.rs`
 * and `client/wire-client/src/lib.rs`: same inputs, same outcomes, so a
 * mismatched pair of binaries fails identically on every frontend.
 */
class ProtocolTest {

    @Test
    fun `current matches the wire constant`() {
        assertEquals(Protocol.CURRENT.toString(), Protocol.WIRE_PROTOCOL_VERSION)
    }

    @Test
    fun `parse accepts major minor only`() {
        assertEquals(ProtocolVersion(1, 3), Protocol.parseVersion("1.3"))
        assertEquals(ProtocolVersion(10, 31), Protocol.parseVersion("10.31"))
        for (bad in listOf("1", "1.2.3", "1.x", "", "v1.2", "1.", ".2", "-1.2", "1 . 2")) {
            try {
                Protocol.parseVersion(bad)
                fail("should not parse: \"$bad\"")
            } catch (e: VersionException) {
                assertTrue(e.message!!, e.message!!.contains("malformed"))
            }
        }
    }

    @Test
    fun `a peer's higher minor is compatible`() {
        val peer = Protocol.checkPeer("1.9")
        assertEquals(ProtocolVersion(1, 9), peer)
        assertTrue(peer.has(3))
    }

    @Test
    fun `a peer's lower minor is compatible but lacks later features`() {
        val peer = Protocol.checkPeer("1.0")
        assertTrue(peer.has(0))
        assertTrue(!peer.has(3))
    }

    @Test
    fun `a different major is refused in both directions`() {
        try {
            Protocol.checkPeer("2.0")
            fail("foreign major must be refused")
        } catch (e: VersionException) {
            val text = e.message!!
            assertTrue(text, text.contains("2.0"))
            assertTrue(text, text.contains(Protocol.WIRE_PROTOCOL_VERSION))
        }
        try {
            Protocol.checkPeer("0.9")
            fail("foreign major must be refused")
        } catch (_: VersionException) {}
    }

    @Test
    fun `the server protocol gate reads the initialize result`() {
        val ok = buildJsonObject {
            put("protocol_version", "1.2")
            put("server", buildJsonObject { put("name", "Kimi Code CLI") })
        }
        assertEquals(ProtocolVersion(1, 2), Protocol.checkServerProtocol(ok))

        val newer = buildJsonObject { put("protocol_version", "1.7") }
        assertEquals(7, Protocol.checkServerProtocol(newer).minor)

        val foreign = buildJsonObject { put("protocol_version", "2.0") }
        try {
            Protocol.checkServerProtocol(foreign)
            fail("foreign major must be refused")
        } catch (e: VersionException) {
            assertTrue(e.message!!, e.message!!.contains("binaries"))
        }

        // An agent too old to declare one at all, or something that is not an
        // agent: must not be reported as an incompatible protocol.
        val silent = buildJsonObject { put("server", buildJsonObject { put("name", "x") }) }
        try {
            Protocol.checkServerProtocol(silent)
            fail("missing declaration must be refused")
        } catch (e: VersionException) {
            assertTrue(e.message!!, e.message!!.contains("names no protocol version"))
        }
    }
}
