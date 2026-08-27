package dev.dvadva.android.session

import dev.dvadva.android.proto.ApprovalKind
import dev.dvadva.android.proto.ApprovalRequest
import dev.dvadva.android.proto.ContentPart
import dev.dvadva.android.proto.ToolCall
import dev.dvadva.android.proto.ToolResult
import dev.dvadva.android.proto.UserInput
import dev.dvadva.android.proto.WireMessage
import kotlinx.serialization.json.buildJsonArray
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.put
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

/** The fold is this app's own (see `Transcript.kt`); these pin its basics. */
class TranscriptFoldTest {

    @Test
    fun `streams text parts into one assistant block`() {
        val t = Transcript()
        t.apply(WireMessage.TurnBegin(UserInput.Text("hi")))
        t.apply(WireMessage.ContentPartMsg(ContentPart.Text("hel")))
        t.apply(WireMessage.ContentPartMsg(ContentPart.Text("lo")))
        val blocks = t.blocks
        assertEquals(2, blocks.size)
        assertTrue(blocks[0] is Block.User)
        assertEquals("hello", (blocks[1] as Block.Assistant).text)
    }

    @Test
    fun `tool call parts accumulate and results close the block`() {
        val t = Transcript()
        t.apply(WireMessage.ToolCallMsg(ToolCall("call-9", "Shell", null)))
        t.apply(WireMessage.ToolCallPartMsg("{\"comma"))
        t.apply(WireMessage.ToolCallPartMsg("nd\":\"ls\"}"))
        t.apply(
            WireMessage.ToolResultMsg(
                ToolResult(
                    "call-9",
                    buildJsonObject {
                        put(
                            "display",
                            buildJsonArray {
                                add(
                                    buildJsonObject {
                                        put("type", "brief")
                                        put("text", "ls: 3 files")
                                    },
                                )
                            },
                        )
                    },
                ),
            ),
        )
        val tool = t.blocks.single() as Block.Tool
        assertEquals("{\"command\":\"ls\"}", tool.arguments)
        assertTrue(tool.done)
        assertEquals("ls: 3 files", tool.brief)
    }

    @Test
    fun `approvals can be answered after the fact`() {
        val t = Transcript()
        t.addApproval(
            ApprovalRequest(
                id = "agent-1",
                toolCallId = "call-1",
                sender = "file",
                action = "write",
                description = "WriteFile x",
                display = emptyList(),
            ),
        )
        t.resolveApproval("agent-1", ApprovalKind.Approve)
        val block = t.blocks.single() as Block.Approval
        assertEquals("approve", block.response)
    }

    @Test
    fun `subagent events are folded with the flag`() {
        val t = Transcript()
        t.apply(
            WireMessage.SubagentEventMsg(
                taskToolCallId = "task-1",
                event = WireMessage.ContentPartMsg(ContentPart.Text("sub says")),
            ),
        )
        val block = t.blocks.single() as Block.Assistant
        assertTrue(block.subagent)
    }
}
