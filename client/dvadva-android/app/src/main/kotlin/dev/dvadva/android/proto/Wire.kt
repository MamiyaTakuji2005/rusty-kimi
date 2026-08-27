package dev.dvadva.android.proto

import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.JsonNull
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.put

/**
 * The dvadva-agent wire protocol: message types and line classification — a
 * Kotlin port of `server/dvadva-agent/src/wire/types.rs` plus `classify_line`
 * from `client/wire-client/src/lib.rs`.
 *
 * Wire shape: newline-delimited JSON-RPC 2.0. Notifications carry
 * `method: "event"`, reverse-RPC requests carry `method: "request"`, and the
 * params of both are one envelope `{"type": "...", "payload": {...}}`.
 */

/** Everything the agent can send us, normalized for the UI layer. */
sealed interface Inbound {
    /** `method: "event"` notification. */
    data class Event(val message: WireMessage) : Inbound

    /** `method: "request"` reverse-RPC that expects a JSON-RPC response. */
    data class Request(val id: String, val message: WireMessage) : Inbound

    /** Response to one of our own requests. */
    data class Response(val id: String, val result: JsonElement?, val error: JsonElement?) : Inbound

    /** The agent process exited (or its stream closed — over a bridge, the daemon's exit trailer said why). */
    data class AgentExited(val reason: String) : Inbound

    /** A line we could not make sense of. */
    data class ProtocolError(val text: String) : Inbound
}

private val json = Json

/**
 * Classify one wire line. Mirrors `classify_line` in `wire-client/src/lib.rs`
 * case for case — including what counts as a protocol error, so a drifted
 * binary fails the same way on every frontend.
 */
fun classifyLine(line: String): Inbound {
    val value = try {
        json.parseToJsonElement(line)
    } catch (e: Exception) {
        return Inbound.ProtocolError("invalid JSON: ${e.message}")
    }
    val obj = value as? JsonObject
        ?: return Inbound.ProtocolError("unclassifiable line: $line")
    val method = (obj["method"] as? JsonPrimitive)?.takeIf { it !is JsonNull }?.content
    val id = (obj["id"] as? JsonPrimitive)?.takeIf { it !is JsonNull && it.isString }?.content
    return when {
        method == "event" -> {
            val params = obj["params"] as? JsonObject
                ?: return Inbound.ProtocolError("event without params")
            try {
                Inbound.Event(parseWireMessage(params))
            } catch (e: Exception) {
                Inbound.ProtocolError("bad event payload: ${e.message}")
            }
        }
        method == "request" && id != null -> {
            val params = obj["params"] as? JsonObject
                ?: return Inbound.ProtocolError("request without params")
            try {
                Inbound.Request(id, parseWireMessage(params))
            } catch (e: Exception) {
                Inbound.ProtocolError("bad request payload: ${e.message}")
            }
        }
        method != null -> Inbound.ProtocolError("unexpected method: $method")
        id != null -> Inbound.Response(
            id = id,
            result = obj["result"],
            error = obj["error"],
        )
        else -> Inbound.ProtocolError("unclassifiable line: $line")
    }
}

// ---------------------------------------------------------------------------
// WireMessage
// ---------------------------------------------------------------------------

/**
 * One typed wire message. The variants the UI actually renders are decoded
 * into data classes; everything the agent streams is one of these. Unknown
 * `type` strings are a parse error, matching the Rust client exactly — the
 * compatibility rule is "a peer's higher minor is safe; ignore what you do
 * not recognize", and ignoring happens *above* this layer (an unrecognized
 * but well-formed message still has a `type` the agent owns).
 */
sealed interface WireMessage {
    data class TurnBegin(val userInput: UserInput) : WireMessage
    data object TurnEnd : WireMessage
    data class SteerInput(val userInput: UserInput) : WireMessage
    data class StepBegin(val n: Long) : WireMessage
    data object StepInterrupted : WireMessage
    data object CompactionBegin : WireMessage
    data object CompactionEnd : WireMessage
    data class StatusUpdate(val status: StatusSnapshot) : WireMessage
    data class ContentPartMsg(val part: ContentPart) : WireMessage
    data class ToolCallMsg(val call: ToolCall) : WireMessage
    data class ToolCallPartMsg(val argumentsPart: String?) : WireMessage
    data class ToolResultMsg(val result: ToolResult) : WireMessage
    data class ApprovalResponseMsg(val response: ApprovalResponse) : WireMessage
    data class SubagentEventMsg(val taskToolCallId: String, val event: WireMessage) : WireMessage
    data class ApprovalRequestMsg(val request: ApprovalRequest) : WireMessage
    data class ToolCallRequestMsg(val request: ToolCallRequest) : WireMessage
    data class NotificationMsg(val notification: WireNotification) : WireMessage
}

/** `UserInput` is untagged on the wire: a bare string or an array of parts. */
sealed interface UserInput {
    data class Text(val text: String) : UserInput
    data class Parts(val parts: List<ContentPart>) : UserInput

    /** Plain-text rendering for a transcript row. */
    fun render(): String = when (this) {
        is Text -> text
        is Parts -> parts.joinToString("\n") { it.renderBrief() }
    }
}

private fun parseUserInput(value: JsonElement): UserInput = when {
    value is JsonNull -> throw IllegalArgumentException("user_input is null")
    value is JsonPrimitive -> UserInput.Text(value.content)
    value is kotlinx.serialization.json.JsonArray ->
        UserInput.Parts(value.map { parseContentPart(it.jsonObject) })
    else -> throw IllegalArgumentException("user_input must be a string or an array of parts")
}

/**
 * Parse the envelope `{"type": "...", "payload": {...}}`. The payload is
 * decoded leniently: fields this client does not use are skipped, but the
 * shapes it does use must match the agent's serde exactly.
 */
fun parseWireMessage(params: JsonObject): WireMessage {
    val type = params.str("type")
        ?: throw IllegalArgumentException("wire message missing type")
    val payload = params["payload"] as? JsonObject
        ?: throw IllegalArgumentException("wire message `$type` missing payload")
    return when (type) {
        "TurnBegin" -> WireMessage.TurnBegin(parseUserInput(payload["user_input"] ?: JsonNull))
        "TurnEnd" -> WireMessage.TurnEnd
        "SteerInput" -> WireMessage.SteerInput(parseUserInput(payload["user_input"] ?: JsonNull))
        "StepBegin" -> WireMessage.StepBegin(payload.str("n")?.toLongOrNull() ?: 0L)
        "StepInterrupted" -> WireMessage.StepInterrupted
        "CompactionBegin" -> WireMessage.CompactionBegin
        "CompactionEnd" -> WireMessage.CompactionEnd
        "StatusUpdate" -> WireMessage.StatusUpdate(parseStatus(payload))
        "ContentPart" -> WireMessage.ContentPartMsg(parseContentPart(payload))
        "ToolCall" -> WireMessage.ToolCallMsg(parseToolCall(payload))
        "ToolCallPart" -> WireMessage.ToolCallPartMsg(payload.str("arguments_part"))
        "ToolResult" -> WireMessage.ToolResultMsg(parseToolResult(payload))
        "ApprovalResponse" -> WireMessage.ApprovalResponseMsg(
            ApprovalResponse(
                requestId = payload.str("request_id") ?: "",
                kind = payload.str("response") ?: "",
            ),
        )
        "SubagentEvent" -> {
            val inner = payload["event"] as? JsonObject
                ?: throw IllegalArgumentException("SubagentEvent missing event")
            WireMessage.SubagentEventMsg(
                taskToolCallId = payload.str("task_tool_call_id") ?: "",
                event = parseWireMessage(inner),
            )
        }
        "ApprovalRequest" -> WireMessage.ApprovalRequestMsg(parseApprovalRequest(payload))
        "ToolCallRequest" -> WireMessage.ToolCallRequestMsg(
            ToolCallRequest(
                id = payload.str("id") ?: "",
                name = payload.str("name") ?: "",
                arguments = payload.str("arguments"),
            ),
        )
        "Notification" -> WireMessage.NotificationMsg(parseNotification(payload))
        else -> throw IllegalArgumentException("unknown message type `$type`")
    }
}

// ---------------------------------------------------------------------------
// Status / approvals / notifications
// ---------------------------------------------------------------------------

/** The `StatusUpdate` payload; every field optional, as on the wire. */
data class StatusSnapshot(
    val contextUsage: Double? = null,
    val contextTokens: Long? = null,
    val maxContextTokens: Long? = null,
    val messageId: String? = null,
    val model: String? = null,
    val yoloEnabled: Boolean? = null,
    val thinking: Boolean? = null,
)

private fun parseStatus(p: JsonObject) = StatusSnapshot(
    contextUsage = p.str("context_usage")?.toDoubleOrNull(),
    contextTokens = p.str("context_tokens")?.toLongOrNull(),
    maxContextTokens = p.str("max_context_tokens")?.toLongOrNull(),
    messageId = p.str("message_id"),
    model = p.str("model"),
    yoloEnabled = p.str("yolo_enabled")?.let { it == "true" },
    thinking = p.str("thinking")?.let { it == "true" },
)

/** The three answers a client may give to an approval request (snake_case on the wire). */
enum class ApprovalKind(val wire: String) {
    Approve("approve"),
    ApproveForSession("approve_for_session"),
    Reject("reject"),
}

/** What the client sends as the JSON-RPC *result* of an approval reverse-request. */
data class ApprovalResponse(val requestId: String, val kind: String) {
    companion object {
        fun of(requestId: String, kind: ApprovalKind) = ApprovalResponse(requestId, kind.wire)
    }
}

/** Serialize an approval answer the way `ApprovalResponse` serde does. */
fun approvalResponseJson(requestId: String, kind: ApprovalKind): JsonObject = buildJsonObject {
    put("request_id", requestId)
    put("response", kind.wire)
}

/** An `ApprovalRequest` reverse-RPC: the agent wants permission before acting. */
data class ApprovalRequest(
    val id: String,
    val toolCallId: String,
    val sender: String,
    val action: String,
    val description: String,
    val display: List<DisplayBlock>,
)

private fun parseApprovalRequest(p: JsonObject) = ApprovalRequest(
    id = p.str("id") ?: "",
    toolCallId = p.str("tool_call_id") ?: "",
    sender = p.str("sender") ?: "",
    action = p.str("action") ?: "",
    description = p.str("description") ?: "",
    display = (p["display"] as? kotlinx.serialization.json.JsonArray)
        ?.map { parseDisplayBlock(it.jsonObject) }
        .orEmpty(),
)

/** A `ToolCallRequest` reverse-RPC: the agent wants *this client* to run a tool (external tools). */
data class ToolCallRequest(val id: String, val name: String, val arguments: String?)

data class WireNotification(
    val category: String,
    val title: String,
    val body: String,
    val severity: String,
)

private fun parseNotification(p: JsonObject) = WireNotification(
    category = p.str("category") ?: "",
    title = p.str("title") ?: "",
    body = p.str("body") ?: "",
    severity = p.str("severity") ?: "",
)

// ---------------------------------------------------------------------------
// Content parts, tool calls, tool results
// ---------------------------------------------------------------------------

/** A `ContentPart`, tagged by `type` on the wire: text / think / image_url / audio_url / video_url. */
sealed interface ContentPart {
    data class Text(val text: String) : ContentPart
    data class Think(val think: String, val encrypted: String?) : ContentPart

    /** The three URL kinds share a shape this client only displays, never decodes further. */
    data class Media(val kind: String, val url: String?) : ContentPart
}

private fun parseContentPart(p: JsonObject): ContentPart {
    val kind = p.str("type") ?: throw IllegalArgumentException("ContentPart missing type")
    return when (kind) {
        "text" -> ContentPart.Text(p.str("text") ?: "")
        "think" -> ContentPart.Think(p.str("think") ?: "", p.str("encrypted"))
        "image_url", "audio_url", "video_url" -> ContentPart.Media(
            kind,
            (p[kind] as? JsonObject)?.str("url"),
        )
        else -> throw IllegalArgumentException("Unknown ContentPart type: $kind")
    }
}

fun ContentPart.renderBrief(): String = when (this) {
    is ContentPart.Text -> text
    is ContentPart.Think -> think
    is ContentPart.Media -> "[$kind]"
}

/** A `ToolCall` event: the agent is calling a tool (may stream arguments in parts). */
data class ToolCall(
    val id: String,
    val name: String,
    val arguments: String?,
)

private fun parseToolCall(p: JsonObject): ToolCall {
    val function = p["function"] as? JsonObject ?: JsonObject(emptyMap())
    return ToolCall(
        id = p.str("id") ?: "",
        name = function.str("name") ?: "",
        arguments = function.str("arguments"),
    )
}

/**
 * A `ToolResult` event. The `return_value` body is kept raw: this client
 * renders only its `display` blocks (brief texts), the way the other
 * frontends do.
 */
data class ToolResult(
    val toolCallId: String,
    val returnValue: JsonObject,
) {
    /** The brief texts of the result's display blocks, for a one-line summary. */
    val briefs: List<String> =
        (returnValue["display"] as? kotlinx.serialization.json.JsonArray)
            ?.mapNotNull { block ->
                val obj = block as? JsonObject ?: return@mapNotNull null
                when (obj.str("type")) {
                    "brief", "shell" -> obj.str("text")
                    else -> null
                }
            }
            .orEmpty()
}

private fun parseToolResult(p: JsonObject) = ToolResult(
    toolCallId = p.str("tool_call_id") ?: "",
    returnValue = p["return_value"] as? JsonObject ?: JsonObject(emptyMap()),
)

/** A `DisplayBlock` (approval previews): typed by `type`, kept shallow. */
data class DisplayBlock(
    val kind: String,
    val text: String?,
    val raw: JsonObject,
)

private fun parseDisplayBlock(p: JsonObject) = DisplayBlock(
    kind = p.str("type") ?: "unknown",
    text = p.str("text"),
    raw = p,
)
