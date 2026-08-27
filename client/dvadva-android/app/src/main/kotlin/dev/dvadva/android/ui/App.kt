package dev.dvadva.android.ui

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.statusBarsPadding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.itemsIndexed
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontStyle
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.lifecycle.viewmodel.compose.viewModel
import dev.dvadva.android.proto.ApprovalKind
import dev.dvadva.android.session.Block
import dev.dvadva.android.session.SessionViewModel
import java.time.Instant
import java.time.ZoneId
import java.time.format.DateTimeFormatter

/**
 * The whole app: a connect screen while [SessionViewModel.Phase.Disconnected]
 * or failed, a conversation screen after that. One conversation per app run —
 * a phone owns the whole screen the way dvadva-tui owns a terminal.
 */
@Composable
fun DvaDvaApp(vm: SessionViewModel = viewModel()) {
    val state by vm.ui.collectAsState()
    if (state.phase == SessionViewModel.Phase.Disconnected ||
        state.phase == SessionViewModel.Phase.Failed && state.blocks.isEmpty()
    ) {
        ConnectScreen(vm, state)
    } else {
        SessionScreen(vm, state)
    }
}

// ---------------------------------------------------------------------------
// Connect screen
// ---------------------------------------------------------------------------

@Composable
private fun ConnectScreen(vm: SessionViewModel, state: SessionViewModel.UiState) {
    var endpoint by rememberSaveable { mutableStateOf("10.7.0.1:9000") }
    var workDir by rememberSaveable { mutableStateOf("") }

    Column(
        modifier = Modifier
            .fillMaxSize()
            .statusBarsPadding()
            .padding(horizontal = 16.dp),
        verticalArrangement = Arrangement.Center,
    ) {
        Text("DvaDva", style = MaterialTheme.typography.headlineMedium, fontWeight = FontWeight.Bold)
        Text(
            "agent over the bridge, through the phone's WireGuard tunnel",
            style = MaterialTheme.typography.bodySmall,
        )
        Spacer(Modifier.height(24.dp))

        OutlinedTextField(
            value = endpoint,
            onValueChange = { endpoint = it },
            label = { Text("bridge endpoint (host:port)") },
            singleLine = true,
            modifier = Modifier.fillMaxWidth(),
        )
        Spacer(Modifier.height(8.dp))

        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            OutlinedButton(onClick = { vm.refreshRemote(endpoint) }, enabled = !state.busy) {
                Text("probe / list sessions")
            }
        }

        state.notice?.let {
            Spacer(Modifier.height(8.dp))
            Text(it, style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.primary)
        }
        state.error?.let {
            Spacer(Modifier.height(8.dp))
            Text(it, style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.error)
        }

        if (state.sessions.isNotEmpty()) {
            Spacer(Modifier.height(16.dp))
            Text("resume", style = MaterialTheme.typography.titleSmall)
            LazyColumn(modifier = Modifier.weight(1f, fill = false)) {
                itemsIndexed(state.sessions) { _, entry ->
                    TextButton(onClick = { vm.connect(endpoint, vm.resumeArgs(entry)) }) {
                        Column {
                            Text(entry.title.ifEmpty { entry.id }, style = MaterialTheme.typography.bodyMedium)
                            Text(
                                "${entry.workDir} · ${relativeTime(entry.updatedAt)}",
                                style = MaterialTheme.typography.bodySmall,
                            )
                        }
                    }
                }
            }
        }

        Spacer(Modifier.height(16.dp))
        OutlinedTextField(
            value = workDir,
            onValueChange = { workDir = it },
            label = { Text("work dir (optional; the daemon's home if empty)") },
            singleLine = true,
            modifier = Modifier.fillMaxWidth(),
        )
        Spacer(Modifier.height(8.dp))
        Button(
            onClick = {
                val args = if (workDir.isBlank()) emptyList() else listOf("-w", workDir.trim())
                vm.connect(endpoint, args)
            },
            enabled = !state.busy,
            modifier = Modifier.fillMaxWidth(),
        ) {
            Text("start new session")
        }
        if (state.busy) {
            Spacer(Modifier.height(8.dp))
            Row(verticalAlignment = Alignment.CenterVertically) {
                CircularProgressIndicator(Modifier.width(18.dp).height(18.dp))
                Spacer(Modifier.width(8.dp))
                Text("connecting…", style = MaterialTheme.typography.bodySmall)
            }
        }
    }
}

private fun relativeTime(unixSeconds: Double): String {
    val delta = System.currentTimeMillis() / 1000.0 - unixSeconds
    return when {
        delta < 60 -> "just now"
        delta < 3600 -> "${(delta / 60).toLong()}m ago"
        delta < 86400 -> "${(delta / 3600).toLong()}h ago"
        delta < 7 * 86400 -> "${(delta / 86400).toLong()}d ago"
        else -> DateTimeFormatter.ofPattern("yyyy-MM-dd")
            .withZone(ZoneId.systemDefault())
            .format(Instant.ofEpochSecond(unixSeconds.toLong()))
    }
}

// ---------------------------------------------------------------------------
// Session screen
// ---------------------------------------------------------------------------

@Composable
private fun SessionScreen(vm: SessionViewModel, state: SessionViewModel.UiState) {
    var input by remember { mutableStateOf("") }
    val listState = rememberLazyListState()

    // Follow the tail as blocks arrive, unless the user scrolled up.
    LaunchedEffect(state.blocks.size, state.blocks.lastOrNull()?.let { (it as? Block.Assistant)?.text?.length }) {
        if (listState.layoutInfo.visibleItemsInfo.any { it.index >= state.blocks.size - 2 }) {
            if (state.blocks.isNotEmpty()) listState.animateScrollToItem(state.blocks.size - 1)
        }
    }

    Scaffold(
        topBar = {
            Surface(tonalElevation = 2.dp) {
                Column(Modifier.statusBarsPadding().padding(horizontal = 16.dp, vertical = 8.dp)) {
                    Row(verticalAlignment = Alignment.CenterVertically) {
                        Text(
                            state.serverName ?: "agent",
                            style = MaterialTheme.typography.titleMedium,
                            fontWeight = FontWeight.SemiBold,
                        )
                        Spacer(Modifier.width(8.dp))
                        StatusDot(state)
                        Spacer(Modifier.weight(1f))
                        TextButton(onClick = { vm.disconnect() }) { Text("disconnect") }
                    }
                    Row {
                        state.status?.model?.let { Text(it, style = MaterialTheme.typography.bodySmall) }
                        state.status?.contextUsage?.let { pct ->
                            Spacer(Modifier.width(8.dp))
                            Text(
                                "ctx ${(pct * 100).toInt()}%",
                                style = MaterialTheme.typography.bodySmall,
                            )
                        }
                        if (state.status?.yoloEnabled == true) {
                            Spacer(Modifier.width(8.dp))
                            Text("yolo", style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.error)
                        }
                    }
                }
            }
        },
        bottomBar = {
            Column(Modifier.navigationBarsPadding().imePadding()) {
                state.approvals.forEach { approval ->
                    ApprovalCard(approval) { kind -> vm.resolveApproval(approval.rpcId, kind) }
                }
                Row(
                    Modifier
                        .fillMaxWidth()
                        .padding(horizontal = 8.dp, vertical = 8.dp),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    OutlinedTextField(
                        value = input,
                        onValueChange = { input = it },
                        placeholder = { Text("message") },
                        modifier = Modifier.weight(1f),
                        maxLines = 4,
                        enabled = state.canSend,
                    )
                    Spacer(Modifier.width(8.dp))
                    if (state.phase == SessionViewModel.Phase.Running) {
                        OutlinedButton(onClick = { vm.cancel() }) { Text("stop") }
                    } else {
                        Button(
                            onClick = {
                                vm.send(input)
                                input = ""
                            },
                            enabled = state.canSend && input.isNotBlank(),
                        ) { Text("send") }
                    }
                }
            }
        },
    ) { padding ->
        LazyColumn(
            state = listState,
            modifier = Modifier
                .fillMaxSize()
                .padding(padding)
                .padding(horizontal = 16.dp),
            contentPadding = androidx.compose.foundation.layout.PaddingValues(vertical = 8.dp),
        ) {
            itemsIndexed(state.blocks) { _, block ->
                BlockRow(block)
                Spacer(Modifier.height(6.dp))
            }
        }
    }
}

@Composable
private fun StatusDot(state: SessionViewModel.UiState) {
    val (color, label) = when (state.phase) {
        SessionViewModel.Phase.Ready -> MaterialTheme.colorScheme.primary to "ready"
        SessionViewModel.Phase.Running -> MaterialTheme.colorScheme.tertiary to "running"
        SessionViewModel.Phase.Replaying -> MaterialTheme.colorScheme.secondary to "replaying"
        SessionViewModel.Phase.Connecting -> MaterialTheme.colorScheme.secondary to "connecting"
        SessionViewModel.Phase.Disconnected -> MaterialTheme.colorScheme.outline to "off"
        SessionViewModel.Phase.Failed -> MaterialTheme.colorScheme.error to "failed"
    }
    Text(label, style = MaterialTheme.typography.labelSmall, color = color)
}

@Composable
private fun ApprovalCard(
    approval: SessionViewModel.PendingApproval,
    onAnswer: (ApprovalKind) -> Unit,
) {
    Card(
        modifier = Modifier
            .fillMaxWidth()
            .padding(horizontal = 8.dp, vertical = 4.dp),
        colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.surfaceVariant),
    ) {
        Column(Modifier.padding(12.dp)) {
            Text(approval.action, style = MaterialTheme.typography.labelMedium, fontWeight = FontWeight.Bold)
            Spacer(Modifier.height(4.dp))
            Text(approval.description, style = MaterialTheme.typography.bodyMedium)
            approval.brief?.let {
                Spacer(Modifier.height(4.dp))
                Text(it, style = MaterialTheme.typography.bodySmall, fontFamily = FontFamily.Monospace)
            }
            Spacer(Modifier.height(8.dp))
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                Button(onClick = { onAnswer(ApprovalKind.Approve) }) { Text("approve" ) }
                OutlinedButton(onClick = { onAnswer(ApprovalKind.ApproveForSession) }) { Text("session") }
                OutlinedButton(onClick = { onAnswer(ApprovalKind.Reject) }) { Text("reject") }
            }
        }
    }
}

@Composable
private fun BlockRow(block: Block) {
    when (block) {
        is Block.User -> Column {
            Text("you", style = MaterialTheme.typography.labelSmall, color = MaterialTheme.colorScheme.secondary)
            Text(block.text, style = MaterialTheme.typography.bodyLarge)
            HorizontalDivider(Modifier.padding(top = 6.dp))
        }
        is Block.Assistant -> Text(
            (if (block.subagent) "· subagent ·\n" else "") + block.text,
            style = MaterialTheme.typography.bodyLarge,
        )
        is Block.Thinking -> Text(
            block.text,
            style = MaterialTheme.typography.bodySmall,
            fontStyle = FontStyle.Italic,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
        is Block.Tool -> Column {
            val mark = if (block.done) "✓" else "…"
            Text(
                "$mark ${block.name}",
                style = MaterialTheme.typography.bodyMedium,
                fontFamily = FontFamily.Monospace,
            )
            block.brief?.let { Text(it, style = MaterialTheme.typography.bodySmall) }
        }
        is Block.Info -> Text(
            block.text,
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
        is Block.Approval -> Column {
            Text(
                "approval: ${block.action} → ${block.response ?: "pending"}",
                style = MaterialTheme.typography.bodySmall,
                fontFamily = FontFamily.Monospace,
            )
        }
    }
}
