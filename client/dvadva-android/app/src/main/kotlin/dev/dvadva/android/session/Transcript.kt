package dev.dvadva.android.session

import dev.dvadva.android.proto.ApprovalKind
import dev.dvadva.android.proto.ApprovalRequest
import dev.dvadva.android.proto.ContentPart
import dev.dvadva.android.proto.StatusSnapshot
import dev.dvadva.android.proto.WireMessage

/**
 * Folds the wire event stream into renderable blocks — a deliberately
 * simplified Kotlin cousin of `client/wire-client/src/transcript.rs`, which
 * stays the reference implementation for desktop frontends. The phone keeps:
 * user turns, streamed assistant text and thinking, tool calls with their
 * briefs, approvals, and info lines; it drops the desktop concepts that have
 * no phone rendering yet (folding, block selection, diff/todo/shell
 * rendering beyond their brief text, per-pane anything).
 */
class Transcript {
    val blocks = mutableListOf<Block>()

    /** The latest `StatusUpdate`, rendered as a status line rather than a block. */
    var status: StatusSnapshot? = null
        private set

    fun apply(msg: WireMessage) = applyInner(msg, subagent = false)

    /** An approval arrived as a reverse-request (not an event); the UI layer calls this. */
    fun addApproval(request: ApprovalRequest) {
        blocks += Block.Approval(
            requestId = request.id,
            action = request.action,
            description = request.description,
            brief = request.display.mapNotNull { it.text }.firstOrNull(),
            response = null,
        )
    }

    /** The user answered; mark the block so a replayed transcript shows the outcome. */
    fun resolveApproval(requestId: String, kind: ApprovalKind) {
        val block = blocks.lastOrNull { it is Block.Approval && it.requestId == requestId }
        if (block is Block.Approval) block.response = kind.wire
    }

    private fun applyInner(msg: WireMessage, subagent: Boolean) {
        when (msg) {
            is WireMessage.TurnBegin -> {
                blocks += Block.User(msg.userInput.render(), subagent)
            }
            is WireMessage.SteerInput -> {
                blocks += Block.User(msg.userInput.render(), subagent)
            }
            is WireMessage.TurnEnd -> {}

            is WireMessage.StepBegin,
            is WireMessage.StepInterrupted,
            -> {}

            is WireMessage.CompactionBegin -> blocks += Block.Info("compacting context…", subagent)
            is WireMessage.CompactionEnd -> blocks += Block.Info("context compacted", subagent)

            is WireMessage.StatusUpdate -> status = msg.status

            is WireMessage.ContentPartMsg -> when (val part = msg.part) {
                is ContentPart.Text -> appendAssistant(part.text, subagent)
                is ContentPart.Think -> appendThinking(part.think, subagent)
                is ContentPart.Media -> appendAssistant("[${part.kind}]", subagent)
            }

            is WireMessage.ToolCallMsg -> {
                blocks += Block.Tool(
                    callId = msg.call.id,
                    name = msg.call.name,
                    arguments = msg.call.arguments.orEmpty(),
                    brief = null,
                    done = false,
                    subagent = subagent,
                )
            }
            is WireMessage.ToolCallPartMsg -> {
                // Arguments stream in parts onto the still-open call.
                val open = blocks.lastOrNull { it is Block.Tool && !it.done && !it.subagent }
                if (open is Block.Tool) {
                    open.arguments += msg.argumentsPart.orEmpty()
                }
            }
            is WireMessage.ToolResultMsg -> {
                val open = blocks.lastOrNull {
                    it is Block.Tool && it.callId == msg.result.toolCallId && !it.done
                }
                if (open is Block.Tool) {
                    open.done = true
                    open.brief = msg.result.briefs.firstOrNull()
                } else {
                    // A result for a call we never saw (mid-stream attach): still show it.
                    blocks += Block.Tool(
                        callId = msg.result.toolCallId,
                        name = "?",
                        arguments = "",
                        brief = msg.result.briefs.firstOrNull(),
                        done = true,
                        subagent = subagent,
                    )
                }
            }

            is WireMessage.ApprovalResponseMsg -> {
                val block = blocks.lastOrNull { it is Block.Approval && it.requestId == msg.response.requestId }
                if (block is Block.Approval) block.response = msg.response.kind
            }
            is WireMessage.ApprovalRequestMsg -> addApproval(msg.request)

            is WireMessage.SubagentEventMsg -> applyInner(msg.event, subagent = true)

            is WireMessage.ToolCallRequestMsg,
            is WireMessage.NotificationMsg,
            -> {}
        }
    }

    private fun appendAssistant(text: String, subagent: Boolean) {
        val last = blocks.lastOrNull()
        if (last is Block.Assistant && last.subagent == subagent) {
            last.text += text
        } else {
            blocks += Block.Assistant(text, subagent)
        }
    }

    private fun appendThinking(text: String, subagent: Boolean) {
        val last = blocks.lastOrNull()
        if (last is Block.Thinking && last.subagent == subagent) {
            last.text += text
        } else {
            blocks += Block.Thinking(text, subagent)
        }
    }
}

/** One renderable transcript row. Mutable fields are stream-accumulated; Compose reads snapshots of the list. */
sealed interface Block {
    val subagent: Boolean

    data class User(val text: String, override val subagent: Boolean) : Block

    class Assistant(var text: String, override val subagent: Boolean) : Block

    class Thinking(var text: String, override val subagent: Boolean) : Block

    class Tool(
        val callId: String,
        val name: String,
        var arguments: String,
        var brief: String?,
        var done: Boolean,
        override val subagent: Boolean,
    ) : Block

    data class Info(val text: String, override val subagent: Boolean) : Block

    class Approval(
        val requestId: String,
        val action: String,
        val description: String,
        val brief: String?,
        var response: String?,
    ) : Block {
        override val subagent: Boolean get() = false
    }
}
