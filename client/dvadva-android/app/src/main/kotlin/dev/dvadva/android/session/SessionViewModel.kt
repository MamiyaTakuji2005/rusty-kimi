package dev.dvadva.android.session

import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import dev.dvadva.android.proto.ApprovalKind
import dev.dvadva.android.proto.Bridge
import dev.dvadva.android.proto.Inbound
import dev.dvadva.android.proto.Protocol
import dev.dvadva.android.proto.WireClient
import dev.dvadva.android.proto.WireMessage
import dev.dvadva.android.proto.approvalResponseJson
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.launch
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.put

/**
 * One conversation per app run (a phone owns the whole screen, like
 * dvadva-tui owns a terminal). The protocol state machine mirrors
 * `client/dvadva-tui/src/agent.rs`: connect → initialize → replay →
 * ready/running/failed, with approvals answered as reverse-RPC.
 *
 * The agent owns the turn; this layer only sends `prompt`/`steer`/`cancel`
 * and folds what comes back. When Phase 2 of PLAN-detached-agent.md lands,
 * reconnect-and-resume goes from "rebuild the transcript" to "reattach to a
 * live agent" — the shape to preserve is here: connection death is a
 * [Phase.Failed] state plus a resume path, never a data loss.
 */
class SessionViewModel : ViewModel() {

    enum class Phase { Disconnected, Connecting, Replaying, Ready, Running, Failed }

    data class PendingApproval(
        val rpcId: String,
        val requestId: String,
        val action: String,
        val description: String,
        val brief: String?,
    )

    data class UiState(
        val phase: Phase = Phase.Disconnected,
        val endpoint: String = "",
        val serverName: String? = null,
        val serverVersion: String? = null,
        val blocks: List<Block> = emptyList(),
        val approvals: List<PendingApproval> = emptyList(),
        val status: StatusSnapshot? = null,
        val error: String? = null,
        /** Non-fatal notices (probe result, listing failures) shown on the connect screen. */
        val notice: String? = null,
        val sessions: List<Bridge.SessionEntry> = emptyList(),
    ) {
        val canSend: Boolean get() = phase == Phase.Ready || phase == Phase.Running
        val busy: Boolean get() = phase == Phase.Connecting || phase == Phase.Replaying
    }

    private val _ui = MutableStateFlow(UiState())
    val ui: StateFlow<UiState> = _ui

    private val transcript = Transcript()

    private var client: WireClient? = null
    private var initId: String? = null
    private var replayId: String? = null
    private var promptId: String? = null

    // --- connection -----------------------------------------------------

    /** Connect and spawn an agent. [args] are verbatim agent CLI args (`-w`, `--session`, …). */
    fun connect(endpoint: String, args: List<String>) {
        if (_ui.value.busy) return
        disconnect()
        _ui.update { it.copy(phase = Phase.Connecting, endpoint = endpoint, error = null, blocks = emptyList(), approvals = emptyList()) }
        transcript.blocks.clear()
        viewModelScope.launch {
            try {
                val c = WireClient.connectTcp(endpoint, args, viewModelScope)
                client = c
                launch { c.inbound.collect(::handle) }
                initId = c.sendRequest(
                    "initialize",
                    buildJsonObject {
                        put("protocol_version", Protocol.WIRE_PROTOCOL_VERSION)
                        put("client", buildJsonObject {
                            put("name", "dvadva-android")
                            put("version", "0.1.0")
                        })
                    },
                )
            } catch (e: Exception) {
                _ui.update { it.copy(phase = Phase.Failed, error = e.message ?: e.toString()) }
            }
        }
    }

    fun disconnect() {
        client?.let { c ->
            client = null
            viewModelScope.launch { c.shutdown() }
        }
        _ui.update {
            if (it.phase != Phase.Failed) it.copy(phase = Phase.Disconnected, approvals = emptyList()) else it
        }
    }

    // --- connect screen helpers ------------------------------------------

    /** Probe the bridge daemon (the connection light) and refresh the session list. */
    fun refreshRemote(endpoint: String) {
        viewModelScope.launch {
            try {
                val probe = Bridge.probe(endpoint)
                val sessions = Bridge.listSessions(endpoint)
                _ui.update { it.copy(notice = probe, sessions = sessions) }
            } catch (e: Exception) {
                _ui.update { it.copy(notice = null, sessions = emptyList(), error = e.message ?: e.toString()) }
            }
        }
    }

    fun resumeArgs(entry: Bridge.SessionEntry): List<String> =
        listOf("--session", entry.id, "-w", entry.workDir)

    // --- the conversation ------------------------------------------------

    fun send(text: String) {
        val trimmed = text.trim()
        if (trimmed.isEmpty() || !_ui.value.canSend) return
        val c = client ?: return
        viewModelScope.launch {
            when (_ui.value.phase) {
                Phase.Ready -> {
                    promptId = c.sendRequest(
                        "prompt",
                        buildJsonObject { put("user_input", trimmed) },
                    )
                    _ui.update { it.copy(phase = Phase.Running) }
                }
                Phase.Running -> {
                    // TurnBegin/SteerInput events echo the input back for display.
                    c.sendRequest("steer", buildJsonObject { put("user_input", trimmed) })
                }
                else -> {}
            }
        }
    }

    fun cancel() {
        val c = client ?: return
        if (_ui.value.phase != Phase.Running) return
        viewModelScope.launch {
            c.sendRequest("cancel", buildJsonObject {})
        }
    }

    fun resolveApproval(rpcId: String, kind: ApprovalKind) {
        val c = client ?: return
        val pending = _ui.value.approvals.firstOrNull { it.rpcId == rpcId } ?: return
        viewModelScope.launch {
            c.respondResult(rpcId, approvalResponseJson(pending.requestId, kind))
            transcript.resolveApproval(pending.requestId, kind)
            _ui.update { state ->
                state.copy(
                    approvals = state.approvals.filterNot { it.rpcId == rpcId },
                    blocks = transcript.blocks.toList(),
                )
            }
        }
    }

    // --- inbound dispatch -------------------------------------------------

    private fun handle(inbound: Inbound) {
        when (inbound) {
            is Inbound.Response -> handleResponse(inbound)
            is Inbound.Event -> handleEvent(inbound.message)
            is Inbound.Request -> handleRequest(inbound.id, inbound.message)
            is Inbound.AgentExited -> {
                transcript.blocks += Block.Info(inbound.reason, false)
                _ui.update {
                    it.copy(
                        phase = Phase.Failed,
                        error = inbound.reason,
                        approvals = emptyList(),
                        blocks = transcript.blocks.toList(),
                    )
                }
            }
            is Inbound.ProtocolError -> {
                transcript.blocks += Block.Info("protocol: ${inbound.text}", false)
                _ui.update { it.copy(blocks = transcript.blocks.toList()) }
            }
        }
    }

    private fun handleResponse(response: Inbound.Response) {
        when (response.id) {
            initId -> {
                initId = null
                val result = response.result as? JsonObject
                if (response.error != null || result == null) {
                    _ui.update {
                        it.copy(
                            phase = Phase.Failed,
                            error = "initialize failed: ${response.error ?: "no result"}",
                        )
                    }
                    return
                }
                // Refuse a protocol we cannot speak before folding any of the
                // result in: everything below assumes the shapes this build knows.
                try {
                    Protocol.checkServerProtocol(result)
                } catch (e: Exception) {
                    _ui.update { it.copy(phase = Phase.Failed, error = e.message) }
                    return
                }
                val c = client ?: return
                viewModelScope.launch {
                    replayId = c.sendRequest("replay", buildJsonObject {})
                }
                _ui.update {
                    it.copy(
                        phase = Phase.Replaying,
                        serverName = (result["server"] as? JsonObject)?.str("name"),
                        serverVersion = result.str("version"),
                    )
                }
            }
            replayId -> {
                replayId = null
                if (response.error != null) {
                    _ui.update { it.copy(phase = Phase.Failed, error = "replay failed: ${response.error}") }
                } else {
                    _ui.update { it.copy(phase = Phase.Ready) }
                }
            }
            promptId -> {
                promptId = null
                if (response.error != null) {
                    transcript.blocks += Block.Info("prompt failed: ${response.error}", false)
                    _ui.update { it.copy(blocks = transcript.blocks.toList()) }
                }
            }
        }
    }

    private fun handleEvent(msg: WireMessage) {
        transcript.apply(msg)
        _ui.update {
            it.copy(
                blocks = transcript.blocks.toList(),
                status = transcript.status,
                phase = when (msg) {
                    // The server echoes both; Running until TurnEnd says otherwise.
                    is WireMessage.TurnBegin -> Phase.Running
                    is WireMessage.TurnEnd -> Phase.Ready
                    else -> it.phase
                },
            )
        }
    }

    private fun handleRequest(id: String, msg: WireMessage) {
        val c = client ?: return
        when (msg) {
            is WireMessage.ApprovalRequestMsg -> {
                val r = msg.request
                transcript.addApproval(r)
                _ui.update { state ->
                    state.copy(
                        blocks = transcript.blocks.toList(),
                        approvals = state.approvals + PendingApproval(
                            rpcId = id,
                            requestId = r.id,
                            action = r.action,
                            description = r.description,
                            brief = r.display.mapNotNull { it.text }.firstOrNull(),
                        ),
                    )
                }
            }
            is WireMessage.ToolCallRequestMsg -> {
                // External tools are a desktop-frontend feature; answer the
                // same way inkvizitor does so the agent degrades gracefully.
                viewModelScope.launch {
                    c.respondError(id, -32000, "External tools are not supported by this client")
                }
            }
            else -> {
                viewModelScope.launch {
                    c.respondError(id, -32601, "method not found")
                }
            }
        }
    }

    override fun onCleared() {
        client?.close()
        client = null
    }
}

private fun JsonObject.str(key: String): String? =
    (this[key] as? kotlinx.serialization.json.JsonPrimitive)
        ?.takeIf { it !is kotlinx.serialization.json.JsonNull }
        ?.content
