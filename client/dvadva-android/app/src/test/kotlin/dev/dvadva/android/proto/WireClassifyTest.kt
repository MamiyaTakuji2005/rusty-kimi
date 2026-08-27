package dev.dvadva.android.proto

import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Test

/**
 * Mirrors the classification tests in `client/wire-client/src/lib.rs`: the
 * same lines, the same verdicts — what counts as a protocol error must not
 * drift between frontends.
 */
class WireClassifyTest {

    private val turnBegin = """{"type":"TurnBegin","payload":{"user_input":"hi"}}"""

    @Test
    fun `classifies events`() {
        val event = """{"jsonrpc":"2.0","method":"event","params":$turnBegin}"""
        val inbound = classifyLine(event)
        assertTrue(inbound is Inbound.Event)
        val msg = (inbound as Inbound.Event).message
        assertTrue(msg is WireMessage.TurnBegin)
        assertEquals(UserInput.Text("hi"), (msg as WireMessage.TurnBegin).userInput)
    }

    @Test
    fun `classifies requests`() {
        val request = """{"jsonrpc":"2.0","method":"request","id":"agent-1","params":$turnBegin}"""
        val inbound = classifyLine(request)
        assertTrue(inbound is Inbound.Request)
        assertEquals("agent-1", (inbound as Inbound.Request).id)
    }

    @Test
    fun `classifies responses`() {
        val ok = """{"jsonrpc":"2.0","id":"client-1","result":{"ok":true}}"""
        val inbound = classifyLine(ok)
        assertTrue(inbound is Inbound.Response)
        val response = inbound as Inbound.Response
        assertEquals("client-1", response.id)
        assertTrue(response.error == null)
    }

    @Test
    fun `classifies protocol garbage`() {
        val cases = listOf(
            "not json",
            """{"jsonrpc":"2.0","method":"event"}""",
            """{"jsonrpc":"2.0","method":"event","params":{"type":"Nope"}}""",
            """{"jsonrpc":"2.0","method":"surprise"}""",
            """{"jsonrpc":"2.0"}""",
        )
        for (line in cases) {
            val inbound = classifyLine(line)
            assertTrue("expected ProtocolError for: $line", inbound is Inbound.ProtocolError)
        }
    }

    @Test
    fun `parses an approval request payload`() {
        val payload =
            """{"id":"agent-42","tool_call_id":"call-9","sender":"file","action":"write",""" +
                """"description":"WriteFile app.kt","display":[{"type":"brief","text":"write 120 B"}]}"""
        val inbound = classifyLine(
            """{"jsonrpc":"2.0","method":"request","id":"agent-42","params":""" +
                """{"type":"ApprovalRequest","payload":$payload}}""",
        )
        assertTrue(inbound is Inbound.Request)
        val msg = (inbound as Inbound.Request).message
        assertTrue(msg is WireMessage.ApprovalRequestMsg)
        val request = (msg as WireMessage.ApprovalRequestMsg).request
        assertEquals("agent-42", request.id)
        assertEquals("write", request.action)
        assertEquals("write 120 B", request.display.first().text)
    }

    @Test
    fun `serializes an approval response the way serde does`() {
        val json = approvalResponseJson("agent-42", ApprovalKind.ApproveForSession)
        assertEquals(
            """{"request_id":"agent-42","response":"approve_for_session"}""",
            json.toString(),
        )
    }

    @Test
    fun `parses streamed content parts and tool calls`() {
        val text = classifyLine(
            """{"jsonrpc":"2.0","method":"event","params":""" +
                """{"type":"ContentPart","payload":{"type":"text","text":"hel"}}}""",
        ) as Inbound.Event
        assertTrue(text.message is WireMessage.ContentPartMsg)
        assertEquals(
            "hel",
            ((text.message as WireMessage.ContentPartMsg).part as ContentPart.Text).text,
        )

        val think = classifyLine(
            """{"jsonrpc":"2.0","method":"event","params":""" +
                """{"type":"ContentPart","payload":{"type":"think","think":"hm"}}}""",
        ) as Inbound.Event
        assertTrue(
            ((think.message as WireMessage.ContentPartMsg).part as ContentPart.Think).think == "hm",
        )

        val call = classifyLine(
            """{"jsonrpc":"2.0","method":"event","params":""" +
                """{"type":"ToolCall","payload":{"type":"function","id":"call-9",""" +
                """"function":{"name":"ReadFile","arguments":"{\"path\":\"x\"}"}}}}""",
        ) as Inbound.Event
        val toolCall = (call.message as WireMessage.ToolCallMsg).call
        assertEquals("call-9", toolCall.id)
        assertEquals("ReadFile", toolCall.name)

        val result = classifyLine(
            """{"jsonrpc":"2.0","method":"event","params":""" +
                """{"type":"ToolResult","payload":{"tool_call_id":"call-9",""" +
                """"return_value":{"display":[{"type":"brief","text":"read 10 lines"}]}}}}""",
        ) as Inbound.Event
        val toolResult = (result.message as WireMessage.ToolResultMsg).result
        assertEquals("call-9", toolResult.toolCallId)
        assertEquals(listOf("read 10 lines"), toolResult.briefs)
    }

    @Test
    fun `an unknown message type is a protocol error`() {
        // Same verdict as the Rust client: an unknown `type` is not silently
        // ignored at the classification layer.
        val inbound = classifyLine(
            """{"jsonrpc":"2.0","method":"event","params":""" +
                """{"type":"SomeFutureMessage","payload":{}}}""",
        )
        assertTrue(inbound is Inbound.ProtocolError)
        assertTrue(
            (inbound as Inbound.ProtocolError).text.contains("unknown message type"),
        )
    }
}
