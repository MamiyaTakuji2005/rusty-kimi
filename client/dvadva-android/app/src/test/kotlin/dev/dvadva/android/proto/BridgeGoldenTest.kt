package dev.dvadva.android.proto

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Test
import java.io.BufferedReader
import java.io.StringReader

/**
 * Golden vectors pinning this port to `client/wire-client/src/bridge.rs` and
 * `remote/dvadva-bridge/src/proto.rs`. The Rust suites assert the same
 * strings byte-for-byte; if one of these fails after a change here, the two
 * framings have drifted — change both sides or neither.
 */
class BridgeGoldenTest {

    @Test
    fun `spawn header shape`() {
        assertEquals(
            """BRIDGE1 {"op":"spawn","args":["-w","/srv"]}""",
            Bridge.spawnHeader(listOf("-w", "/srv")),
        )
        assertEquals("""BRIDGE1 {"op":"list_sessions"}""", Bridge.listSessionsHeader())
        assertEquals("""BRIDGE1 {"op":"version"}""", Bridge.versionHeader())
    }

    @Test
    fun `reply decodes`() {
        val ok = Bridge.decodeReply("""BRIDGE1 {"ok":true}""")
        assertTrue(ok.ok)
        assertNull(ok.sessions)

        val err = Bridge.decodeReply("""BRIDGE1 {"ok":false,"error":"boom"}""")
        assertEquals(false, err.ok)
        assertEquals("boom", err.error)

        val listing = Bridge.decodeReply(
            """BRIDGE1 {"ok":true,"sessions":[""" +
                """{"id":"a","title":"t","work_dir":"/w","updated_at":1.5}]}""",
        )
        val sessions = listing.sessions!!
        assertEquals(1, sessions.size)
        assertEquals("a", sessions[0].id)
        assertEquals("/w", sessions[0].workDir)
        assertEquals(1.5, sessions[0].updatedAt, 0.0)

        try {
            Bridge.decodeReply("""{"ok":true}""")
            fail("missing magic must be refused")
        } catch (e: BridgeException) {
            assertTrue(e.message!!, e.message!!.contains("not a bridge reply"))
        }
    }

    @Test
    fun `a daemon from another major is named as one`() {
        // What the app sees when the far end of the tunnel is a stale (or too
        // new) daemon. It must not read as a transport fault.
        try {
            Bridge.decodeReply("""BRIDGE2 {"ok":true}""")
            fail("foreign magic must be refused")
        } catch (e: BridgeException) {
            assertTrue(e.message!!, e.message!!.contains("BRIDGE2"))
            assertTrue(e.message!!, e.message!!.contains("not compatible"))
        }

        // Whereas an HTTP server on the wrong port is still just not a bridge.
        try {
            Bridge.decodeReply("HTTP/1.1 200 OK")
            fail("non-bridge line must be refused")
        } catch (e: BridgeException) {
            assertTrue(e.message!!, e.message!!.contains("not a bridge reply"))
        }
    }

    @Test
    fun `a daemon that names no frame protocol is still readable`() {
        val reply = Bridge.decodeReply("""BRIDGE1 {"ok":true,"version":"1.8.0"}""")
        assertEquals("1.8.0", reply.version)
        assertNull(reply.proto)
    }

    @Test
    fun `exit trailer carries the daemon's reason`() {
        val trailer = Bridge.exitTrailer(
            """BRIDGE1 {"ok":false,"error":"agent exited: exit status: 2\nwork dir does not exist"}""",
        )
        assertTrue(trailer!!, trailer.contains("exit status: 2"))
        assertTrue(trailer.contains("work dir does not exist"))

        assertEquals("remote agent exited", Bridge.exitTrailer("""BRIDGE1 {"ok":true}"""))
        // Wire-protocol JSON never starts with the magic.
        assertNull(Bridge.exitTrailer("""{"jsonrpc":"2.0","id":"client-1","result":{}}"""))
    }

    @Test
    fun `read frame line caps an endless peer`() {
        val reader = BufferedReader(StringReader("x".repeat(Bridge.MAX_LINE_BYTES + 1024)))
        try {
            Bridge.readFrameLine(reader)
            fail("oversized frame must be refused")
        } catch (e: BridgeException) {
            assertTrue(e.message!!, e.message!!.contains("size limit"))
        }
    }

    @Test
    fun `read frame line reports premature close`() {
        val mid = BufferedReader(StringReader("BRIDGE1 {"))
        try {
            Bridge.readFrameLine(mid)
            fail("close mid-frame must be refused")
        } catch (e: BridgeException) {
            assertTrue(e.message!!, e.message!!.contains("mid-frame"))
        }

        val empty = BufferedReader(StringReader(""))
        try {
            Bridge.readFrameLine(empty)
            fail("close without reply must be refused")
        } catch (e: BridgeException) {
            assertTrue(e.message!!, e.message!!.contains("without replying"))
        }
    }

    @Test
    fun `read frame line keeps the newline out and tolerates crlf`() {
        val reader = BufferedReader(StringReader("""BRIDGE1 {"ok":true}""" + "\r\n"))
        assertEquals("""BRIDGE1 {"ok":true}""", Bridge.readFrameLine(reader))
    }

    @Test
    fun `the magic's digit is the frame protocol's major`() {
        assertEquals("BRIDGE1", Bridge.MAGIC)
        assertEquals("1.0", Bridge.PROTOCOL_VERSION)
        assertTrue(Bridge.MAGIC.endsWith(Bridge.PROTOCOL_VERSION.substringBefore('.')))
        assertEquals(64 * 1024, Bridge.MAX_LINE_BYTES)
    }
}
