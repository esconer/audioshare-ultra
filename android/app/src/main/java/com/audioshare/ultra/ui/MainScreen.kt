package com.audioshare.ultra.ui

import android.Manifest
import android.app.Activity
import android.content.BroadcastReceiver
import android.content.ClipData
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.content.pm.PackageManager
import android.media.projection.MediaProjectionConfig
import android.media.projection.MediaProjectionManager
import android.net.Uri
import android.os.Build
import android.os.PowerManager
import android.provider.Settings
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.animation.AnimatedVisibility
import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.clickable
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.layout.ExperimentalLayoutApi
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.rounded.*
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalLifecycleOwner
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.core.content.ContextCompat
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.repeatOnLifecycle
import androidx.lifecycle.viewmodel.compose.viewModel
import com.audioshare.ultra.service.AudioEngineService
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import kotlin.properties.ReadWriteProperty
import kotlin.reflect.KProperty
import kotlin.math.abs

private fun usToMs(us: Long): String = "%.1f".format(us / 1000.0)

private fun codecModeName(mode: Long): String = when (mode.toInt()) {
    0 -> "Lossless LPC"
    1 -> "Raw PCM16"
    2 -> "VBR Compressed"
    3 -> "DTX Silence"
    4 -> "Opus RLD"
    else -> "Unknown"
}

/**
 * Telemetry-heavy hero. All 16 Hz snapshot reads are confined to this leaf
 * (and the two other telemetry leaves), keeping them out of the Connect/EQ/
 * Party card bodies. The snapshot is read via a lambda passed from the parent.
 */
@Composable
private fun HeroStageSection(
    telemetry: () -> TelemetrySnapshot,
    isConnected: Boolean,
    isPartyCasting: Boolean,
    sharePhoneMic: Boolean,
    hostAddress: String,
    sleepRemainingMins: Int,
    latencyDebug: Boolean
) {
    val t = telemetry()
    val heroGlow = when {
        isPartyCasting && !isConnected -> Color(0xFFA855F7)
        sharePhoneMic && !isConnected -> StudioColors.Accent
        !isConnected -> StudioColors.Muted
        else -> StudioColors.LiveGlow
    }
    HeroAudioStage(
        glowColor = heroGlow,
        energyFraction = if (isConnected) {
            t.getOrElse(24) { 0L }.toInt().coerceIn(0, 100) / 100f
        } else 0f
    ) {
        val authErr = t.getOrElse(5) { 0L }
        val idleSecs = t.getOrElse(6) { 0L }
        val dtxSilent = t.getOrElse(7) { 0L } == 1L
        val hostVol = t.getOrElse(8) { 100L }
        val groupDelay = t.getOrElse(9) { 0L }
        val codecMode = t.getOrElse(19) { 0L }
        val partyPacked = t.getOrElse(22) { 0L }
        val partyListeners = (partyPacked and 0xFFL).toInt()
        val partyCodec = (partyPacked shr 8) and 0xFFL
        val partySyncMs = t.getOrElse(23) { 0L }

        val (statusTitle, dotColor) = when {
            isPartyCasting && !isConnected ->
                "Party Broadcast • $partyListeners Peer(s) • ${codecModeName(partyCodec)} • Sync +${partySyncMs}ms" to Color(0xFFA855F7)
            sharePhoneMic && !isConnected ->
                "Wireless Mic Active -> $hostAddress" to StudioColors.Accent
            !isConnected ->
                "Ready to Connect" to StudioColors.Muted
            authErr > 5 ->
                "Wrong Encryption PIN ($authErr rejected)" to StudioColors.DangerSoft
            idleSecs >= 3 ->
                "Host Signal Lost (${idleSecs}s) — Waiting..." to StudioColors.WarnFg
            dtxSilent ->
                "Smart DTX Standby (-90 dBFS BT Keep-Alive)" to StudioColors.AccentSoft
            else ->
                "Streaming 48 kHz • ${codecModeName(codecMode)}" to StudioColors.OkFg
        }
        Row(
            modifier = Modifier.fillMaxWidth(),
            horizontalArrangement = Arrangement.spacedBy(8.dp),
            verticalAlignment = Alignment.CenterVertically
        ) {
            PulsingStatusDot(
                active = isConnected || isPartyCasting || sharePhoneMic,
                color = dotColor
            )
            Text(
                text = statusTitle,
                style = MaterialTheme.typography.titleSmall,
                fontWeight = FontWeight.SemiBold,
                modifier = Modifier.weight(1f)
            )
            if (isConnected) {
                StatusBadge(
                    text = "Vol ${hostVol}%",
                    containerColor = Color(0xFF1E3A5F),
                    contentColor = Color(0xFF93C5FD)
                )
            }
            if (sleepRemainingMins > 0) {
                StatusBadge(
                    text = "${sleepRemainingMins}m",
                    containerColor = StudioColors.AccentDeep,
                    contentColor = StudioColors.AccentSoft
                )
            }
        }
        if (isConnected) {
            val vuNow = t.getOrElse(24) { 0L }.toInt().coerceIn(0, 100)
            FluidAudioVisualizer(vuPct = vuNow, isLive = !dtxSilent)
        }
        if (isConnected) {
            Row(
                modifier = Modifier
                    .fillMaxWidth()
                    .horizontalScroll(rememberScrollState()),
                horizontalArrangement = Arrangement.spacedBy(6.dp)
            ) {
                StatusBadge("Pkts: ${t[0]}")
                StatusBadge("FEC: ${t[2]}")
                StatusBadge("Drift: ${t[4]} ppm")
                if (groupDelay > 0) {
                    StatusBadge("Sync: +${groupDelay}ms")
                }
                if (latencyDebug) {
                    val estMs = t.getOrElse(20) { 0L } / 10.0
                    val targetMs = t.getOrElse(14) { 0L } / 1000.0
                    val bufferedMs = t.getOrElse(15) { 0L } / 1000.0
                    StatusBadge(
                        text = "Est ${"%.1f".format(estMs)}ms • FIFO ${"%.1f".format(bufferedMs)}/${"%.1f".format(targetMs)}ms",
                        containerColor = Color(0xFF14532D),
                        contentColor = Color(0xFF86EFAC)
                    )
                }
            }
        }
    }
}

/** Telemetry drawer: all 16 Hz counter/latency reads live here, not in MainScreen. */
@Composable
private fun TelemetryDrawerSection(
    telemetry: () -> TelemetrySnapshot,
    expanded: Boolean,
    latencyDebug: Boolean,
    btDelayOverrideMs: Int,
    btEarbudsDetected: Boolean,
    onToggleExpanded: () -> Unit,
    onToggleLatencyDebug: () -> Unit,
    onBtOverrideChanged: (Int) -> Unit,
    onBtOverrideFinished: () -> Unit,
    onBtAuto: () -> Unit,
    diagnosticsActions: @Composable () -> Unit
) {
    val t = telemetry()
    ProDrawerHeader(
        title = "Real-Time Telemetry & Latency Breakdown",
        badge = "Drift ${t[4]} ppm • FEC ${t[2]}",
        expanded = expanded,
        onToggle = onToggleExpanded
    )

    AnimatedVisibility(visible = expanded) {
        StudioCard {
            Row(
                modifier = Modifier.fillMaxWidth(),
                horizontalArrangement = Arrangement.SpaceBetween,
                verticalAlignment = Alignment.CenterVertically
            ) {
                Text(
                    "Hardware Sequencer Counters",
                    style = MaterialTheme.typography.titleSmall,
                    fontWeight = FontWeight.SemiBold
                )
                FilterChip(
                    selected = latencyDebug,
                    onClick = onToggleLatencyDebug,
                    label = { Text("Stage Breakdown") }
                )
            }

            Row(modifier = Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                MetricTile("RX Packets", "${t[0]}", Modifier.weight(1f))
                MetricTile("Lossless Frames", "${t[1]}", Modifier.weight(1f))
            }
            Row(modifier = Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                MetricTile("FEC Healed", "${t[2]}", Modifier.weight(1f))
                MetricTile("WSOLA PLC", "${t[3]}", Modifier.weight(1f))
            }
            Row(modifier = Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                MetricTile("Clock Drift", "${t[4]} ppm", Modifier.weight(1f))
                MetricTile("AAudio DAC", "${t.getOrElse(10) { 0L }} ms", Modifier.weight(1f))
            }
            Row(modifier = Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                MetricTile("Host Silence", "${t.getOrElse(6) { 0L }} s", Modifier.weight(1f))
                MetricTile("Auth Errors", "${t.getOrElse(5) { 0L }}", Modifier.weight(1f))
            }

            if (btEarbudsDetected) {
                val activeBtMs = t.getOrElse(17) { 185L }
                    .toInt().coerceIn(40, 350)
                StatusBadge(
                    text = "Bluetooth Audio Active: Reporting +${activeBtMs}ms A/V Sync Offset to PC",
                    containerColor = Color(0xFF14532D),
                    contentColor = Color(0xFF86EFAC)
                )
                Row(
                    modifier = Modifier.fillMaxWidth(),
                    horizontalArrangement = Arrangement.SpaceBetween,
                    verticalAlignment = Alignment.CenterVertically
                ) {
                    Text(
                        "BT Lip-Sync Calibration: ${if (btDelayOverrideMs == 0) "Auto (${activeBtMs}ms)" else "${btDelayOverrideMs}ms"}",
                        style = MaterialTheme.typography.bodySmall
                    )
                    TextButton(onClick = onBtAuto) { Text("Auto") }
                }
                Slider(
                    value = (if (btDelayOverrideMs == 0) activeBtMs else btDelayOverrideMs).toFloat(),
                    onValueChange = { onBtOverrideChanged(it.toInt().coerceIn(40, 350)) },
                    onValueChangeFinished = onBtOverrideFinished,
                    valueRange = 40f..350f
                )
            }

            if (latencyDebug) {
                val packetUs = t.getOrElse(11) { 0L }
                val jitterUs = t.getOrElse(12) { 0L }
                val adaptiveUs = t.getOrElse(13) { 0L }
                val targetUs = t.getOrElse(14) { 0L }
                val bufferedUs = t.getOrElse(15) { 0L }
                val groupUs = t.getOrElse(16) { 0L }
                val btMs = t.getOrElse(17) { 0L }
                val prebufferReady = t.getOrElse(18) { 0L } == 1L
                val codecMode = t.getOrElse(19) { 0L }
                val estMs = t.getOrElse(20) { 0L } / 10.0
                val codecLookaheadMs = t.getOrElse(21) { 0L } / 10.0
                val fillPct = if (targetUs > 0) {
                    ((bufferedUs.toDouble() / targetUs) * 100.0).coerceIn(0.0, 100.0)
                } else {
                    0.0
                }
                val btLabel = if (btMs > 0) "$btMs ms (fixed A2DP)" else "not routed"
                val prebufferLabel = if (prebufferReady) "READY" else "FILLING"

                HorizontalDivider(color = Color(0xFF243047))
                Text(
                    "Live Latency Breakdown",
                    style = MaterialTheme.typography.titleSmall,
                    fontWeight = FontWeight.SemiBold
                )
                Text(
                    "Estimated one-way  : ${"%.1f".format(estMs)} ms",
                    fontFamily = FontFamily.Monospace,
                    color = MaterialTheme.colorScheme.tertiary
                )
                Text("Sender packetization: ${usToMs(packetUs)} ms", fontFamily = FontFamily.Monospace, style = MaterialTheme.typography.bodySmall)
                Text("Network jitter     : ${usToMs(jitterUs)} ms", fontFamily = FontFamily.Monospace, style = MaterialTheme.typography.bodySmall)
                Text("Adaptive cushion   : ${usToMs(adaptiveUs)} ms", fontFamily = FontFamily.Monospace, style = MaterialTheme.typography.bodySmall)
                Text("Target FIFO        : ${usToMs(targetUs)} ms", fontFamily = FontFamily.Monospace, style = MaterialTheme.typography.bodySmall)
                Text(
                    "Buffered right now : ${usToMs(bufferedUs)} ms (${"%.0f".format(fillPct)}%)",
                    fontFamily = FontFamily.Monospace,
                    style = MaterialTheme.typography.bodySmall
                )
                Text("Group sync addition: +${usToMs(groupUs)} ms", fontFamily = FontFamily.Monospace, style = MaterialTheme.typography.bodySmall)
                Text("DAC queue          : ${t.getOrElse(10) { 0L }} ms", fontFamily = FontFamily.Monospace, style = MaterialTheme.typography.bodySmall)
                Text("Codec lookahead    : ${"%.1f".format(codecLookaheadMs)} ms", fontFamily = FontFamily.Monospace, style = MaterialTheme.typography.bodySmall)
                Text("Bluetooth estimate : $btLabel", fontFamily = FontFamily.Monospace, style = MaterialTheme.typography.bodySmall)
                Text("Prebuffer / Codec  : $prebufferLabel • ${codecModeName(codecMode)}", fontFamily = FontFamily.Monospace, style = MaterialTheme.typography.bodySmall)
                Text(
                    "Estimate = packetization + target FIFO + max(DAC, BT, output) + codec lookahead.",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant
                )
            }

            HorizontalDivider(color = Color(0xFF243047))
            diagnosticsActions()
        }
    }
}

/** Live party-host telemetry (only composed while a party cast is active). */
@Composable
private fun PartyTelemetrySection(
    telemetry: () -> TelemetrySnapshot,
    partySilenceWarning: Boolean,
    showPartyQrDialog: Boolean,
    pinCode: String,
    onSilenceWarning: (Boolean) -> Unit,
    onShowQr: () -> Unit,
    onDismissQr: () -> Unit,
    onCopied: () -> Unit
) {
    val t = telemetry()
    val partyPacked = t.getOrElse(22) { 0L }
    val partyListeners = (partyPacked and 0xFFL).toInt()
    val partyCodec = (partyPacked shr 8) and 0xFFL
    val partyPort = ((partyPacked shr 16) and 0xFFFFL).let { if (it > 0) it else 65530L }
    val partySyncMs = t.getOrElse(23) { 0L }
    // stats[22] bits 40..47 carry the live capture VU peak. Zero for ~12 polls
    // (~3 s) means DRM/OEM mute is feeding silence to peers.
    val partyVu = ((partyPacked ushr 40) and 0xFFL).toInt().coerceIn(0, 100)

    LaunchedEffect(partyVu == 0) {
        if (partyVu == 0) {
            delay(3000)
            onSilenceWarning(true)
        } else {
            onSilenceWarning(false)
        }
    }

    Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
        HorizontalDivider(color = Color(0xFF243047))
        Text(
            "Live Party Host Telemetry (Port $partyPort)",
            style = MaterialTheme.typography.labelMedium,
            color = MaterialTheme.colorScheme.secondary
        )
        Text(
            "Captured Audio Level",
            style = MaterialTheme.typography.labelSmall,
            color = StudioColors.Muted
        )
        FluidAudioVisualizer(vuPct = partyVu, isLive = partyVu > 0)
        if (partySilenceWarning) {
            StatusBadge(
                text = "No app audio detected — start music or check DRM/mute",
                containerColor = StudioColors.WarnBg,
                contentColor = StudioColors.WarnFg
            )
        }
        Row(
            modifier = Modifier.fillMaxWidth(),
            horizontalArrangement = Arrangement.spacedBy(8.dp)
        ) {
            MetricTile("Connected Peers", "$partyListeners", Modifier.weight(1f))
            MetricTile("Party Codec", codecModeName(partyCodec), Modifier.weight(1f))
            MetricTile("Group Sync", "+${partySyncMs} ms", Modifier.weight(1f))
        }
        FilledTonalButton(
            onClick = onShowQr,
            modifier = Modifier.fillMaxWidth()
        ) {
            Icon(Icons.Rounded.QrCode, null, Modifier.size(18.dp))
            Spacer(Modifier.width(8.dp))
            Text("Show Party QR Code (Port $partyPort)")
        }
        Text(
            "Tip: For 3+ peers or outdoors, enable this phone's Mobile Hotspot for a 1-hop direct link with half the Wi-Fi lag.",
            style = MaterialTheme.typography.bodySmall,
            color = StudioColors.Muted
        )
    }
    if (showPartyQrDialog) {
        PartyQrDialog(
            partyPort = partyPort.toInt(),
            pinCode = pinCode,
            onDismiss = onDismissQr,
            onCopied = onCopied
        )
    }
}

/**
 * Shared Copy/Share diagnostics actions used by both the Tools bottom sheet and
 * the Connect-tab telemetry drawer. [onDone] runs before any Snackbar is shown
 * so the Tools sheet can dismiss itself first and keep the message visible.
 */
@Composable
private fun DiagnosticsActionsRow(
    snackbarHostState: SnackbarHostState,
    onDone: () -> Unit
) {
    val context = LocalContext.current
    val scope = rememberCoroutineScope()
    Row(
        modifier = Modifier.fillMaxWidth(),
        horizontalArrangement = Arrangement.spacedBy(8.dp)
    ) {
        OutlinedButton(
            onClick = {
                val diag = runCatching { AudioEngineService.nativeGetDiagnostics() }
                    .getOrNull()?.takeIf { it.isNotBlank() }
                // Dismiss first so the Snackbar is not hidden behind the sheet.
                onDone()
                if (diag == null) {
                    scope.launch {
                        snackbarHostState.showSnackbar("Diagnostics unavailable")
                    }
                } else {
                    val diagChars = diag.length
                    val cm = context.getSystemService(Context.CLIPBOARD_SERVICE)
                        as? android.content.ClipboardManager
                    if (cm != null) {
                        cm.setPrimaryClip(
                            ClipData.newPlainText("AudioShare Ultra diagnostics", diag)
                        )
                    }
                    scope.launch {
                        snackbarHostState.showSnackbar("Diagnostics copied ($diagChars chars)")
                    }
                }
            },
            modifier = Modifier.weight(1f)
        ) {
            Icon(
                Icons.Rounded.ContentCopy,
                contentDescription = null,
                modifier = Modifier.size(16.dp)
            )
            Spacer(Modifier.width(8.dp))
            Text("Copy Diag", maxLines = 1)
        }
        OutlinedButton(
            onClick = {
                val diag = runCatching { AudioEngineService.nativeGetDiagnostics() }
                    .getOrNull()?.takeIf { it.isNotBlank() }
                onDone()
                if (diag != null) {
                    // Capture the nullable local into a non-null local before use.
                    val diagText: String = diag
                    val sendIntent = Intent(Intent.ACTION_SEND).apply {
                        type = "text/plain"
                        putExtra(Intent.EXTRA_TEXT, diagText)
                    }
                    runCatching {
                        context.startActivity(
                            Intent.createChooser(sendIntent, "Share diagnostics")
                        )
                    }
                }
            },
            modifier = Modifier.weight(1f)
        ) {
            Icon(
                Icons.Rounded.Share,
                contentDescription = null,
                modifier = Modifier.size(16.dp)
            )
            Spacer(Modifier.width(8.dp))
            Text("Share Diag", maxLines = 1)
        }
    }
}

@OptIn(ExperimentalMaterial3Api::class, ExperimentalLayoutApi::class)
@Composable
fun MainScreen(vm: AudioShareViewModel = viewModel()) {
    val context = LocalContext.current
    val lifecycleOwner = LocalLifecycleOwner.current
    val state by vm.uiState.collectAsState()

    /**
     * Reads are served from the collected [state] so Compose observes every
     * change; writes go through [AudioShareViewModel.update] so they survive
     * rotation. Purely local UI state (dialogs, launchers) stays as `remember`.
     */
    fun <T> vmField(
        read: (AudioShareUiState) -> T,
        write: (AudioShareUiState, T) -> AudioShareUiState
    ): ReadWriteProperty<Any?, T> = object : ReadWriteProperty<Any?, T> {
        override fun getValue(thisRef: Any?, property: KProperty<*>): T = read(state)
        override fun setValue(thisRef: Any?, property: KProperty<*>, value: T) {
            vm.update { write(it, value) }
        }
    }

    val snackbarHostState = remember { SnackbarHostState() }
    val scope = rememberCoroutineScope()
    var showQrScanner by remember { mutableStateOf(false) }
    var showToolsSheet by remember { mutableStateOf(false) }
    var showPartyQrDialog by remember { mutableStateOf(false) }
    // partySilenceWarning is now owned by the ViewModel (survives rotation).
    var pendingEncryptedHost by remember { mutableStateOf<String?>(null) }
    var promptPinInput by remember { mutableStateOf("") }

    val cameraPermLauncher = rememberLauncherForActivityResult(
        ActivityResultContracts.RequestPermission()
    ) { granted ->
        if (granted) {
            showQrScanner = true
        } else {
            scope.launch {
                snackbarHostState.showSnackbar("Camera permission needed to scan the PC QR code")
            }
        }
    }
    fun launchQrScanner() {
        val granted = ContextCompat.checkSelfPermission(
            context, Manifest.permission.CAMERA
        ) == PackageManager.PERMISSION_GRANTED
        if (granted) showQrScanner = true else cameraPermLauncher.launch(Manifest.permission.CAMERA)
    }
    val prefs = remember {
        context.getSharedPreferences("audioshare_ultra_prefs", Context.MODE_PRIVATE)
    }
    val powerManager = remember {
        context.getSystemService(Context.POWER_SERVICE) as PowerManager
    }
    var isBatteryUnrestricted by remember {
        mutableStateOf(
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.M) {
                powerManager.isIgnoringBatteryOptimizations(context.packageName)
            } else {
                true
            }
        )
    }
    val batteryOptLauncher = rememberLauncherForActivityResult(
        ActivityResultContracts.StartActivityForResult()
    ) { _ ->
        // Re-check only after the OS dialog closes; never grant optimistically.
        isBatteryUnrestricted = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.M) {
            powerManager.isIgnoringBatteryOptimizations(context.packageName)
        } else {
            true
        }
    }

    // Every field below is backed by AudioShareUiState via vmField. Initial
    // values (prefs / service state) are loaded once in the ViewModel.
    var hostAddress by vmField({ it.hostAddress }, { s, v -> s.copy(hostAddress = v) })
    var pinCode by vmField({ it.pinCode }, { s, v -> s.copy(pinCode = v) })
    var selectedTab by vmField({ it.selectedTab }, { s, v -> s.copy(selectedTab = v) }) // 0=Connect, 1=Audio & Tune, 2=Mic & Party
    var isConnected by vmField({ it.isConnected }, { s, v -> s.copy(isConnected = v) })
    var preampGainDb by vmField({ it.preampGainDb }, { s, v -> s.copy(preampGainDb = v) })
    var bassDb by vmField({ it.bassDb }, { s, v -> s.copy(bassDb = v) })
    var vocalDb by vmField({ it.vocalDb }, { s, v -> s.copy(vocalDb = v) })
    var trebleDb by vmField({ it.trebleDb }, { s, v -> s.copy(trebleDb = v) })
    var nightMode by vmField({ it.nightMode }, { s, v -> s.copy(nightMode = v) })
    var crossfeed by vmField({ it.crossfeed }, { s, v -> s.copy(crossfeed = v) })
    var gamingLock by vmField({ it.gamingLock }, { s, v -> s.copy(gamingLock = v) })
    // OFF (default) keeps the raw low-latency AAudio path. ON switches to the
    // shared mixer so OEM Dolby Atmos / Dirac / system EQ can post-process.
    var dolbyEffects by vmField({ it.dolbyEffects }, { s, v -> s.copy(dolbyEffects = v) })
    var pendingInternalAudioCast by vmField(
        { it.pendingInternalAudioCast },
        { s, v -> s.copy(pendingInternalAudioCast = v) }
    )
    var partySilenceWarning by vmField(
        { it.partySilenceWarning },
        { s, v -> s.copy(partySilenceWarning = v) }
    )
    var latencyDebug by vmField({ it.latencyDebug }, { s, v -> s.copy(latencyDebug = v) })
    var channelRole by vmField({ it.channelRole }, { s, v -> s.copy(channelRole = v) })
    // 0=Smart Auto, 1=Studio Lossless 16-bit, 2=Low-Latency VBR,
    // 3=Max Stability (10ms 2.4GHz), 4=Studio Master 24-bit
    var qualityMode by vmField({ it.qualityMode }, { s, v -> s.copy(qualityMode = v) })
    // Target for the compressed profiles; command codes 30..33 = 96/160/256/320 kbps.
    var compressedBitrateKbps by vmField(
        { it.compressedBitrateKbps },
        { s, v -> s.copy(compressedBitrateKbps = v) }
    )
    // false = pure-Rust VBR (default), true = bundled pure-Rust Opus RLD.
    var useOpus by vmField({ it.useOpus }, { s, v -> s.copy(useOpus = v) })
    var btDelayOverrideMs by vmField(
        { it.btDelayOverrideMs },
        { s, v -> s.copy(btDelayOverrideMs = v) }
    )
    // The service owns mic / party-cast lifecycles; these mirror its broadcasts.
    var sharePhoneMic by vmField({ it.sharePhoneMic }, { s, v -> s.copy(sharePhoneMic = v) })
    var castPhoneInternalAudio by vmField(
        { it.castPhoneInternalAudio },
        { s, v -> s.copy(castPhoneInternalAudio = v) }
    )
    var forwardPartyToPc by vmField(
        { it.forwardPartyToPc },
        { s, v -> s.copy(forwardPartyToPc = v) }
    )
    // Default ON to eliminate the ~60 ms same-room speaker slapback echo.
    var muteHostSpeakerInParty by vmField(
        { it.muteHostSpeakerInParty },
        { s, v -> s.copy(muteHostSpeakerInParty = v) }
    )
    var sleepTimerMinutes by vmField(
        { it.sleepRemainingMins },
        { s, v -> s.copy(sleepRemainingMins = v) }
    ) // 0=Off, 15, 30, 60m
    // 25 immutable telemetry slots (see TelemetrySnapshot in AudioShareViewModel).
    var telemetry by vmField({ it.telemetry }, { s, v -> s.copy(telemetry = v) })
    var btEarbudsDetected by vmField(
        { it.btEarbudsDetected },
        { s, v -> s.copy(btEarbudsDetected = v) }
    )

    // Progressive Disclosure Drawer States ("Simple First, Complex When Needed")
    var showManualConnect by vmField(
        { it.showManualConnect },
        { s, v -> s.copy(showManualConnect = v) }
    )
    var showTelemetryDrawer by vmField(
        { it.showTelemetryDrawer },
        { s, v -> s.copy(showTelemetryDrawer = v) }
    )
    var showEqDrawer by vmField({ it.showEqDrawer }, { s, v -> s.copy(showEqDrawer = v) })
    var showCodecDrawer by vmField({ it.showCodecDrawer }, { s, v -> s.copy(showCodecDrawer = v) })

    /** Zero-allocation live DSP update: JNI only, no SharedPreferences or intents. */
    fun applyDspLive() {
        AudioEngineService.nativeSetGainDb(preampGainDb)
        AudioEngineService.nativeConfigureDsp(
            bassDb, vocalDb, trebleDb, channelRole,
            crossfeed, nightMode, gamingLock, dolbyEffects
        )
    }

    /** Flush the final DSP values to prefs and the service after a gesture ends. */
    fun syncAndSaveDsp() {
        prefs.edit()
            .putFloat("gain_db", preampGainDb)
            .putFloat("bass_db", bassDb)
            .putFloat("vocal_db", vocalDb)
            .putFloat("treble_db", trebleDb)
            .putBoolean("night_mode", nightMode)
            .putBoolean("crossfeed", crossfeed)
            .putBoolean("gaming_lock", gamingLock)
            .putBoolean("dolby_fx", dolbyEffects)
            .putInt("channel_role", channelRole)
            .apply()
        applyDspLive()
        // Changing the preamp unmutes audio via nativeSetGainDb above. Clear
        // the service's notification mute flag in the same gesture so the
        // notification cannot stay stuck on "MUTED".
        if (isConnected) {
            context.startService(
                Intent(context, AudioEngineService::class.java).apply {
                    action = AudioEngineService.ACTION_CLEAR_LOCAL_MUTE
                }
            )
        }
    }

    fun setDolbyEffects(enabled: Boolean) {
        dolbyEffects = enabled
        prefs.edit().putBoolean("dolby_fx", enabled).apply()
        // The native engine reopens AAudio when this atomic flips, so the
        // toggle is live and does not require reconnecting the stream.
        if (isConnected) {
            val intent = Intent(context, AudioEngineService::class.java).apply {
                action = "ACTION_SET_DOLBY"
                putExtra("DOLBY_ENABLED", enabled)
            }
            context.startService(intent)
        }
    }

    // The service is the single owner of the projection session. Mirror its
    // state so a system-revoked projection (or a failed start) flips the
    // switch back even when this screen was not the caller.
    DisposableEffect(context) {
        val receiver = object : BroadcastReceiver() {
            override fun onReceive(ctx: Context?, intent: Intent?) {
                if (intent?.action == AudioEngineService.ACTION_PARTY_CAST_STATE) {
                    val active = intent.getBooleanExtra(
                        AudioEngineService.EXTRA_PARTY_CAST_ACTIVE,
                        false
                    )
                    // The service restores the saved music volume on every
                    // stop path, so the UI must not touch it here.
                    castPhoneInternalAudio = active
                } else if (intent?.action == AudioEngineService.ACTION_PHONE_MIC_STATE) {
                    sharePhoneMic = intent.getBooleanExtra(
                        AudioEngineService.EXTRA_PHONE_MIC_ACTIVE,
                        false
                    )
                } else if (intent?.action == AudioEngineService.ACTION_SLEEP_TIMER_STATE) {
                    sleepTimerMinutes = intent.getIntExtra(
                        AudioEngineService.EXTRA_SLEEP_REMAINING_MINS,
                        0
                    )
                } else if (intent?.action == AudioEngineService.ACTION_STREAM_STATE) {
                    isConnected = intent.getBooleanExtra(
                        AudioEngineService.EXTRA_STREAM_ACTIVE,
                        false
                    )
                    val err = intent.getStringExtra(AudioEngineService.EXTRA_STREAM_ERROR)
                    if (!err.isNullOrBlank()) {
                        scope.launch { snackbarHostState.showSnackbar(err) }
                    }
                }
            }
        }
        val filter = IntentFilter().apply {
            addAction(AudioEngineService.ACTION_PARTY_CAST_STATE)
            addAction(AudioEngineService.ACTION_PHONE_MIC_STATE)
            addAction(AudioEngineService.ACTION_SLEEP_TIMER_STATE)
            addAction(AudioEngineService.ACTION_STREAM_STATE)
        }
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            context.registerReceiver(receiver, filter, Context.RECEIVER_NOT_EXPORTED)
        } else {
            @Suppress("DEPRECATION")
            context.registerReceiver(receiver, filter)
        }
        onDispose {
            try {
                context.unregisterReceiver(receiver)
            } catch (_: Exception) {
            }
        }
    }

    // Route every "connect" gesture through one place. True connection state
    // comes back via ACTION_STREAM_STATE, so we never claim success optimistically.
    fun connectToHost(addr: String, isUsb: Boolean? = null, pin: String) {
        hostAddress = addr
        pinCode = pin
        prefs.edit()
            .putString("last_host_addr", addr)
            .putString("pin_code", pin)
            .apply()
        val intent = Intent(context, AudioEngineService::class.java).apply {
            putExtra("HOST_ADDR", addr)
            // A null value omits IS_USB so the service falls back to
            // auto-detecting RNDIS/USB hosts from the address.
            if (isUsb != null) putExtra("IS_USB", isUsb)
            putExtra("GAIN_DB", preampGainDb)
            putExtra("PIN", pin)
        }
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            context.startForegroundService(intent)
        } else {
            context.startService(intent)
        }
    }


    fun startPhoneMicInService() {
        val intent = Intent(context, AudioEngineService::class.java).apply {
            action = AudioEngineService.ACTION_START_PHONE_MIC
            putExtra("HOST_ADDR", hostAddress)
            putExtra("PIN", pinCode)
        }
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            context.startForegroundService(intent)
        } else {
            context.startService(intent)
        }
    }

    val micPermissionLauncher = rememberLauncherForActivityResult(
        contract = ActivityResultContracts.RequestPermission()
    ) { granted ->
        if (granted) {
            startPhoneMicInService()
        } else {
            sharePhoneMic = false
        }
    }

    fun createAudioCaptureProjectionIntent(): Intent {
        val mgr = context.getSystemService(Context.MEDIA_PROJECTION_SERVICE) as MediaProjectionManager
        return if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
            // Android 14+ defaults createScreenCaptureIntent() to the
            // single-app video window picker, which cannot feed
            // AudioPlaybackCapture. Request the default-display audio mix.
            mgr.createScreenCaptureIntent(MediaProjectionConfig.createConfigForDefaultDisplay())
        } else {
            mgr.createScreenCaptureIntent()
        }
    }

    val projectionLauncher = rememberLauncherForActivityResult(
        contract = ActivityResultContracts.StartActivityForResult()
    ) { result ->
        val resultData = result.data
        if (result.resultCode == Activity.RESULT_OK && resultData != null && Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            // The service releases the single global hardware-mic transmitter
            // in startPartyCast() before handing the mic to the caster.
            if (sharePhoneMic) {
                sharePhoneMic = false
            }
            val svcIntent = Intent(context, AudioEngineService::class.java).apply {
                action = AudioEngineService.ACTION_START_PARTY_CAST
                putExtra("PROJECTION_RESULT_CODE", result.resultCode)
                putExtra("PROJECTION_DATA", resultData)
                putExtra("TARGET_HOSTS", if (forwardPartyToPc) hostAddress else "")
            }
            try {
                if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                    context.startForegroundService(svcIntent)
                } else {
                    context.startService(svcIntent)
                }
                // Service applies the local speaker mute from party_mute_local
                // once the caster is live; do not manage volume from the UI.
                castPhoneInternalAudio = true
            } catch (_: Exception) {
                castPhoneInternalAudio = false
            }
        } else {
            castPhoneInternalAudio = false
        }
    }

    // Android 10+ playback capture requires RECORD_AUDIO. Ask at the moment
    // the user enables internal-audio casting instead of at app launch, so a
    // plain playback session never requests microphone permission.
    val castAudioPermissionLauncher = rememberLauncherForActivityResult(
        contract = ActivityResultContracts.RequestPermission()
    ) { granted ->
        val pendingCast = pendingInternalAudioCast
        pendingInternalAudioCast = false
        if (granted && pendingCast) {
            projectionLauncher.launch(createAudioCaptureProjectionIntent())
        } else if (!granted) {
            castPhoneInternalAudio = false
        }
    }

    // Sleep timer is now owned by AudioEngineService: the countdown survives
    // screen-off and rotation, and publishes ACTION_SLEEP_TIMER_STATE here.

    // 1-Tap auto-discovery: the native UDP beacon scanner runs in the Rust JNI layer
    var discoveredHosts by remember { mutableStateOf<List<String>>(emptyList()) }
    LaunchedEffect(Unit) {
        lifecycleOwner.repeatOnLifecycle(Lifecycle.State.STARTED) {
            while (true) {
                isConnected = AudioEngineService.isStreaming
                discoveredHosts = withContext(Dispatchers.IO) {
                    runCatching { AudioEngineService.nativeGetDiscoveredHosts() }
                        .getOrDefault("").lines().filter { it.isNotBlank() }
                }
                delay(2000)
            }
        }
    }

    // Poll native telemetry (4 Hz idle / ~16 Hz live). Remote Party media keys
    // are dispatched by AudioEngineService, which owns the single media receiver.
    // The 300 s silence auto-disconnect now lives in the service watchdog so it
    // also fires while the screen is off.
    LaunchedEffect(isConnected, castPhoneInternalAudio, sharePhoneMic, dolbyEffects) {
        lifecycleOwner.repeatOnLifecycle(Lifecycle.State.STARTED) {
            while (isConnected || castPhoneInternalAudio || sharePhoneMic) {
                if (isConnected && !AudioEngineService.isStreaming) {
                    isConnected = false
                }
                val raw = withContext(Dispatchers.IO) {
                    AudioEngineService.nativeGetTelemetry()
                }
                vm.updateTelemetry(raw)

                if (!isConnected) {
                    delay(250)
                    continue
                }
                // ~16Hz refresh while connected keeps the hero visualizer fluid.
                delay(60)
            }
        }
    }

    Scaffold(
        snackbarHost = { SnackbarHost(snackbarHostState) },
        bottomBar = {
            NavigationBar(containerColor = Color(0xFF0E131D)) {
                NavigationBarItem(
                    selected = selectedTab == 0,
                    onClick = { selectedTab = 0 },
                    icon = { Icon(Icons.Rounded.Wifi, contentDescription = null) },
                    label = { Text("Connect") }
                )
                NavigationBarItem(
                    selected = selectedTab == 1,
                    onClick = { selectedTab = 1 },
                    icon = { Icon(Icons.Rounded.Equalizer, contentDescription = null) },
                    label = { Text("Audio & EQ") }
                )
                NavigationBarItem(
                    selected = selectedTab == 2,
                    onClick = { selectedTab = 2 },
                    icon = { Icon(Icons.Rounded.Podcasts, contentDescription = null) },
                    label = { Text("Mic & Party") }
                )
            }
        },
        topBar = {
            TopAppBar(
                title = {
                    Row(
                        verticalAlignment = Alignment.CenterVertically,
                        horizontalArrangement = Arrangement.spacedBy(8.dp)
                    ) {
                        val topBadgeText = when {
                            castPhoneInternalAudio -> "• ON AIR"
                            sharePhoneMic -> "• MIC LIVE"
                            isConnected -> "• LIVE"
                            else -> "• STANDBY"
                        }
                        val anyActive = isConnected || castPhoneInternalAudio || sharePhoneMic
                        Text("AudioShare Ultra", fontWeight = FontWeight.Bold)
                        StatusBadge(
                            text = topBadgeText,
                            containerColor = if (anyActive) Color(0xFF14532D) else Color(0xFF1A2436),
                            contentColor = if (anyActive) Color(0xFF86EFAC) else Color(0xFF94A3B8)
                        )
                    }
                },
                actions = {
                    FilterChip(
                        selected = latencyDebug,
                        onClick = {
                            val next = !latencyDebug
                            latencyDebug = next
                            if (next) showTelemetryDrawer = true
                            prefs.edit().putBoolean("latency_debug", next).apply()
                        },
                        label = { Text("HUD") },
                        modifier = Modifier.padding(end = 8.dp)
                    )
                },
                colors = TopAppBarDefaults.topAppBarColors(
                    containerColor = MaterialTheme.colorScheme.background
                )
            )
        }
    ) { padding ->
        Column(
            modifier = Modifier
                .fillMaxSize()
                .padding(padding)
                .padding(horizontal = 14.dp, vertical = 6.dp)
                .verticalScroll(rememberScrollState()),
            verticalArrangement = Arrangement.spacedBy(12.dp)
        ) {
            // =====================================================================
            // ATMOSPHERIC HERO AUDIO STAGE (single calm focal point)
            // =====================================================================
            HeroStageSection(
                telemetry = { telemetry },
                isConnected = isConnected,
                isPartyCasting = castPhoneInternalAudio,
                sharePhoneMic = sharePhoneMic,
                hostAddress = hostAddress,
                sleepRemainingMins = sleepTimerMinutes,
                latencyDebug = latencyDebug
            )

            // Tab navigation moved to the ergonomic bottom NavigationBar above.

            if (selectedTab == 0) {
                // 0. 1-Tap OEM Battery / Doze Exemption Banner
                if (!isBatteryUnrestricted && Build.VERSION.SDK_INT >= Build.VERSION_CODES.M) {
                    StudioCard {
                        Row(
                            modifier = Modifier.fillMaxWidth(),
                            horizontalArrangement = Arrangement.SpaceBetween,
                            verticalAlignment = Alignment.CenterVertically
                        ) {
                            Row(
                                modifier = Modifier.weight(1f),
                                verticalAlignment = Alignment.CenterVertically,
                                horizontalArrangement = Arrangement.spacedBy(6.dp)
                            ) {
                                Icon(
                                    Icons.Rounded.Bolt,
                                    contentDescription = null,
                                    modifier = Modifier.size(16.dp),
                                    tint = StudioColors.WarnFg
                                )
                                Text(
                                    "Disable Battery Throttling for 0-Jitter Screen-Off Audio",
                                    style = MaterialTheme.typography.bodySmall
                                )
                            }
                            OutlinedButton(onClick = {
                                val i = Intent(Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS).apply {
                                    data = Uri.parse("package:${context.packageName}")
                                }
                                try {
                                    batteryOptLauncher.launch(i)
                                } catch (_: Exception) {
                                    // Some OEMs omit the Doze-exemption screen; fall back to
                                    // app settings and still re-check the real OS state on return.
                                    try {
                                        batteryOptLauncher.launch(
                                            Intent(Settings.ACTION_APPLICATION_DETAILS_SETTINGS).apply {
                                                data = Uri.parse("package:${context.packageName}")
                                            }
                                        )
                                    } catch (_: Exception) {
                                        // Leave the banner visible; no optimistic grant.
                                    }
                                }
                            }) { Text("Allow") }
                        }
                    }
                }

                // =====================================================================
                // 1. SIMPLE HERO CONNECTION CARD (1-Tap Discovered Hosts + Primary CTA)
                // =====================================================================
                StudioCard {
                    Row(
                        modifier = Modifier.fillMaxWidth(),
                        horizontalArrangement = Arrangement.SpaceBetween,
                        verticalAlignment = Alignment.CenterVertically
                    ) {
                        Text(
                            "Host Connection",
                            style = MaterialTheme.typography.titleMedium,
                            fontWeight = FontWeight.SemiBold,
                            modifier = Modifier.weight(1f)
                        )
                        StatusBadge(hostAddress)
                    }

                    if (discoveredHosts.isNotEmpty()) {
                        Text(
                            "Available Hosts (Tap to Connect)",
                            style = MaterialTheme.typography.labelMedium,
                            color = MaterialTheme.colorScheme.secondary
                        )
                        discoveredHosts.forEach { entry ->
                            val parts = entry.split("|")
                            val name = parts.getOrNull(0) ?: entry
                            val addr = parts.getOrNull(1) ?: entry
                            val mode = parts.getOrNull(2) ?: "WiFi"
                            // 4th JNI CSV field is the beacon's enc=0/1 flag.
                            val encrypted = parts.getOrNull(3) == "1"
                            val kind = if (name.contains("Party")) "Party Phone" else "PC"
                            FilledTonalButton(
                                onClick = {
                                    if (encrypted && pinCode.isBlank()) {
                                        pendingEncryptedHost = entry
                                        promptPinInput = ""
                                    } else {
                                        connectToHost(
                                            addr,
                                            mode.equals("USB", ignoreCase = true),
                                            pinCode
                                        )
                                    }
                                },
                                modifier = Modifier.fillMaxWidth()
                            ) {
                                Icon(
                                    if (mode.equals("USB", ignoreCase = true)) Icons.Rounded.Usb
                                    else Icons.Rounded.Wifi,
                                    contentDescription = null,
                                    modifier = Modifier.size(18.dp)
                                )
                                Spacer(Modifier.width(8.dp))
                                Text("$kind: $name ($addr)", modifier = Modifier.weight(1f))
                                if (encrypted) {
                                    Icon(
                                        Icons.Rounded.Lock,
                                        contentDescription = "Encrypted",
                                        modifier = Modifier.size(16.dp)
                                    )
                                }
                            }
                        }
                    } else {
                        Text(
                            "Scanning LAN & USB for AudioShare Ultra beacons...",
                            style = MaterialTheme.typography.bodySmall,
                            color = MaterialTheme.colorScheme.onSurfaceVariant
                        )
                    }

                    Button(
                        onClick = {
                            if (isConnected) {
                                val intent = Intent(context, AudioEngineService::class.java).apply {
                                    action = AudioEngineService.ACTION_STOP_CLIENT_ONLY
                                }
                                context.startService(intent)
                                isConnected = false
                            } else {
                                // Primary connect: omit IS_USB so the service
                                // auto-detects USB RNDIS vs Wi-Fi hosts.
                                connectToHost(addr = hostAddress, isUsb = null, pin = pinCode)
                                // Real result arrives via ACTION_STREAM_STATE.
                            }
                        },
                        modifier = Modifier
                            .fillMaxWidth()
                            .height(48.dp),
                        colors = if (isConnected) {
                            ButtonDefaults.buttonColors(containerColor = Color(0xFFB91C1C))
                        } else {
                            ButtonDefaults.buttonColors(
                                containerColor = MaterialTheme.colorScheme.primaryContainer,
                                contentColor = MaterialTheme.colorScheme.onPrimaryContainer
                            )
                        }
                    ) {
                        Icon(
                            if (isConnected) Icons.Rounded.Stop else Icons.Rounded.PlayArrow,
                            contentDescription = null
                        )
                        Spacer(Modifier.width(8.dp))
                        Text(
                            if (isConnected) "Disconnect Stream" else "Connect Lossless Stream",
                            fontWeight = FontWeight.Bold
                        )
                    }

                    // Sleek 3-pill quick action dock: secondary tools are in the
                    // ModalBottomSheet below so the primary flow stays uncluttered.
                    Row(
                        modifier = Modifier.fillMaxWidth(),
                        horizontalArrangement = Arrangement.spacedBy(8.dp)
                    ) {
                        FilledTonalButton(
                            onClick = {
                                if (!isConnected) {
                                    connectToHost("127.0.0.1:65530", true, pinCode)
                                } else {
                                    hostAddress = "127.0.0.1:65530"
                                }
                            },
                            shape = RoundedCornerShape(14.dp),
                            modifier = Modifier.weight(1f)
                        ) {
                            Icon(Icons.Rounded.Usb, contentDescription = null, modifier = Modifier.size(16.dp))
                            Spacer(Modifier.width(4.dp))
                            Text("USB", maxLines = 1)
                        }
                        FilledTonalButton(
                            onClick = { launchQrScanner() },
                            shape = RoundedCornerShape(14.dp),
                            modifier = Modifier.weight(1f)
                        ) {
                            Icon(Icons.Rounded.QrCodeScanner, contentDescription = null, modifier = Modifier.size(16.dp))
                            Spacer(Modifier.width(4.dp))
                            Text("QR", maxLines = 1)
                        }
                        FilledTonalButton(
                            onClick = { showToolsSheet = true },
                            shape = RoundedCornerShape(14.dp),
                            modifier = Modifier.weight(1f)
                        ) {
                            Icon(Icons.Rounded.Tune, contentDescription = null, modifier = Modifier.size(16.dp))
                            Spacer(Modifier.width(4.dp))
                            Text("Tools", maxLines = 1)
                        }
                    }

                    // Progressive Disclosure: Manual Host IP & PIN fields.
                    // Keep open only when manually expanded or no LAN host is
                    // discovered; encrypted discovered hosts trigger the 1-tap
                    // PIN dialog automatically on tap.
                    AnimatedVisibility(visible = showManualConnect || discoveredHosts.isEmpty()) {
                        Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                            HorizontalDivider(color = Color(0xFF243047))
                            if (showManualConnect || discoveredHosts.isEmpty()) {
                                OutlinedTextField(
                                    value = hostAddress,
                                    onValueChange = { hostAddress = it },
                                    label = { Text("Host IP:Port (USB RNDIS or Wi-Fi)") },
                                    singleLine = true,
                                    modifier = Modifier.fillMaxWidth()
                                )
                            }
                            OutlinedTextField(
                                value = pinCode,
                                onValueChange = {
                                    pinCode = it
                                    showManualConnect = true
                                    prefs.edit().putString("pin_code", it).apply()
                                },
                                label = { Text("PIN (Optional, ChaCha20-Poly1305 AEAD)") },
                                singleLine = true,
                                modifier = Modifier.fillMaxWidth()
                            )
                        }
                    }
                }

                // =====================================================================
                // 3. PRO TELEMETRY & STAGE-BY-STAGE LATENCY DRAWER (On-Demand)
                // =====================================================================
                TelemetryDrawerSection(
                    telemetry = { telemetry },
                    expanded = showTelemetryDrawer,
                    latencyDebug = latencyDebug,
                    btDelayOverrideMs = btDelayOverrideMs,
                    btEarbudsDetected = btEarbudsDetected,
                    onToggleExpanded = { showTelemetryDrawer = !showTelemetryDrawer },
                    onToggleLatencyDebug = {
                        val next = !latencyDebug
                        latencyDebug = next
                        prefs.edit().putBoolean("latency_debug", next).apply()
                    },
                    onBtOverrideChanged = { v ->
                        btDelayOverrideMs = v
                        // Zero-allocation live JNI update while dragging.
                        AudioEngineService.nativeReportBluetoothDelayMs(v)
                    },
                    onBtOverrideFinished = {
                        prefs.edit().putInt("bt_delay_override_ms", btDelayOverrideMs).apply()
                        context.startService(
                            Intent(context, AudioEngineService::class.java).apply {
                                action = AudioEngineService.ACTION_SET_BT_OFFSET
                            }
                        )
                    },
                    onBtAuto = {
                        btDelayOverrideMs = 0
                        prefs.edit().putInt("bt_delay_override_ms", 0).apply()
                        context.startService(
                            Intent(context, AudioEngineService::class.java).apply {
                                action = AudioEngineService.ACTION_SET_BT_OFFSET
                            }
                        )
                    },
                    // No sheet to dismiss here; the Scaffold SnackbarHost is shared.
                    diagnosticsActions = {
                        DiagnosticsActionsRow(
                            snackbarHostState = snackbarHostState,
                            onDone = {}
                        )
                    }
                )
            } // End Tab 0

            if (selectedTab == 1) {
                // 4 intent-driven host quality cards, surfaced directly on the tab.
                Row(
                    modifier = Modifier.fillMaxWidth(),
                    horizontalArrangement = Arrangement.spacedBy(8.dp)
                ) {
                    ModePresetCard(
                        title = "Smart Auto",
                        subtitle = "Lossless / Opus",
                        badge = "Default",
                        selected = qualityMode == 0,
                        onClick = {
                            qualityMode = 0
                            prefs.edit().putInt("quality_mode", 0).apply()
                            AudioEngineService.nativeSendMediaCommand(1)
                        },
                        modifier = Modifier.weight(1f)
                    )
                    ModePresetCard(
                        title = "Studio Lossless",
                        subtitle = "Bit-exact 0ms",
                        badge = if (qualityMode == 4) "24-Bit" else "16-Bit",
                        selected = qualityMode == 1 || qualityMode == 4,
                        onClick = {
                            if (qualityMode != 1 && qualityMode != 4) {
                                qualityMode = 1
                                prefs.edit().putInt("quality_mode", 1).apply()
                                AudioEngineService.nativeSendMediaCommand(2)
                            }
                        },
                        modifier = Modifier.weight(1f)
                    )
                }
                Row(
                    modifier = Modifier.fillMaxWidth(),
                    horizontalArrangement = Arrangement.spacedBy(8.dp)
                ) {
                    ModePresetCard(
                        title = "Smooth Wi-Fi",
                        subtitle = "Best for 2.4G / BT",
                        badge = "${if (useOpus) "Opus" else "VBR"} ${compressedBitrateKbps}k",
                        selected = qualityMode == 2,
                        onClick = {
                            qualityMode = 2
                            prefs.edit().putInt("quality_mode", 2).apply()
                            AudioEngineService.nativeSendMediaCommand(3)
                        },
                        modifier = Modifier.weight(1f)
                    )
                    ModePresetCard(
                        title = "Max Range",
                        subtitle = "Through walls",
                        badge = "10 ms",
                        selected = qualityMode == 3,
                        onClick = {
                            qualityMode = 3
                            prefs.edit().putInt("quality_mode", 3).apply()
                            AudioEngineService.nativeSendMediaCommand(4)
                        },
                        modifier = Modifier.weight(1f)
                    )
                }
                SectionEyebrow("Sound Profile")

                // =====================================================================
                // 1. SIMPLE SOUND PRESETS & MASTER PRE-AMP (Always Visible)
                // =====================================================================
                StudioCard {
                    Row(
                        modifier = Modifier.fillMaxWidth(),
                        horizontalArrangement = Arrangement.SpaceBetween,
                        verticalAlignment = Alignment.CenterVertically
                    ) {
                        Text(
                            "Sound Profile & Volume Boost",
                            style = MaterialTheme.typography.titleMedium,
                            fontWeight = FontWeight.SemiBold
                        )
                        TextButton(onClick = {
                            preampGainDb = 0f
                            bassDb = 0f
                            vocalDb = 0f
                            trebleDb = 0f
                            syncAndSaveDsp()
                        }) {
                            Text("Reset All")
                        }
                    }

                    FlowRow(
                        modifier = Modifier.fillMaxWidth(),
                        horizontalArrangement = Arrangement.spacedBy(8.dp),
                        verticalArrangement = Arrangement.spacedBy(4.dp)
                    ) {
                        listOf(
                            Triple("Flat Studio", 0f, 0f) to 0f,
                            Triple("Bass Boost", 6.5f, 0f) to 1.5f,
                            Triple("Vocal Clarity", -2f, 5.5f) to 2f,
                            Triple("FPS Footsteps", -3f, 6f) to 4.5f
                        ).forEach { (b_v, t) ->
                            val isSelected = abs(bassDb - b_v.second) < 0.05f &&
                                abs(vocalDb - b_v.third) < 0.05f &&
                                abs(trebleDb - t) < 0.05f
                            FilterChip(
                                selected = isSelected,
                                onClick = {
                                    bassDb = b_v.second
                                    vocalDb = b_v.third
                                    trebleDb = t
                                    syncAndSaveDsp()
                                },
                                label = { Text(b_v.first) }
                            )
                        }
                    }

                    StudioFaderRow(
                        title = "Studio Pre-Amp",
                        freqLabel = "True-Peak Limited",
                        value = preampGainDb,
                        valueRange = -12.0f..24.0f,
                        onValueChange = {
                            preampGainDb = it
                            applyDspLive()
                        },
                        onValueChangeFinished = { syncAndSaveDsp() }
                    )
                }

                // =====================================================================
                // 2. SIMPLE ENHANCEMENTS CARD (4 Clean Switch Rows)
                // =====================================================================
                StudioCard {
                    Text(
                        "Audio Enhancements",
                        style = MaterialTheme.typography.titleMedium,
                        fontWeight = FontWeight.SemiBold
                    )

                    SettingSwitchRow(
                        title = "Night Mode DRC",
                        subtitle = "Boost quiet dialogue & footsteps, tame loud explosions",
                        checked = nightMode,
                        onCheckedChange = {
                            nightMode = it
                            syncAndSaveDsp()
                        }
                    )
                    SettingSwitchRow(
                        title = "3D Headphone Stage",
                        subtitle = "Binaural studio monitor crossfeed (0.0 ms lookahead)",
                        checked = crossfeed,
                        onCheckedChange = {
                            crossfeed = it
                            syncAndSaveDsp()
                        }
                    )
                    SettingSwitchRow(
                        title = "5 ms Competitive Gaming Lock",
                        subtitle = "Locks jitter buffer to minimum floor & bypasses group sync",
                        checked = gamingLock,
                        onCheckedChange = {
                            gamingLock = it
                            syncAndSaveDsp()
                        }
                    )
                    SettingSwitchRow(
                        title = "Dolby Atmos / System Effects",
                        subtitle = "Route through Android shared mixer for OEM Dolby & EQ",
                        checked = dolbyEffects,
                        onCheckedChange = { setDolbyEffects(it) }
                    )
                }

                // =====================================================================
                // 3. PRO DRAWER 1: Custom 3-Band Parametric EQ & Speaker Role
                // =====================================================================
                val roleName = listOf("Stereo", "Left", "Right", "Vocal", "Surround")[channelRole.coerceIn(0, 4)]
                val eqActive = abs(bassDb) >= 0.05f || abs(vocalDb) >= 0.05f || abs(trebleDb) >= 0.05f
                ProDrawerHeader(
                    title = "Custom 3-Band Parametric EQ & Speaker Role",
                    badge = "${if (eqActive) "Custom EQ" else "Flat EQ"} • $roleName",
                    expanded = showEqDrawer,
                    onToggle = { showEqDrawer = !showEqDrawer }
                )

                AnimatedVisibility(visible = showEqDrawer) {
                    StudioCard {
                        StudioFaderRow(
                            title = "Sub-Bass",
                            freqLabel = "80 Hz",
                            value = bassDb,
                            valueRange = -12.0f..12.0f,
                            onValueChange = {
                                bassDb = it
                                applyDspLive()
                            },
                            onValueChangeFinished = { syncAndSaveDsp() }
                        )
                        StudioFaderRow(
                            title = "Vocal Presence",
                            freqLabel = "2.5 kHz",
                            value = vocalDb,
                            valueRange = -12.0f..12.0f,
                            onValueChange = {
                                vocalDb = it
                                applyDspLive()
                            },
                            onValueChangeFinished = { syncAndSaveDsp() }
                        )
                        StudioFaderRow(
                            title = "Treble Air",
                            freqLabel = "10 kHz",
                            value = trebleDb,
                            valueRange = -12.0f..12.0f,
                            onValueChange = {
                                trebleDb = it
                                applyDspLive()
                            },
                            onValueChangeFinished = { syncAndSaveDsp() }
                        )

                        HorizontalDivider(color = Color(0xFF243047))
                        Text(
                            "Spatial Speaker Role",
                            style = MaterialTheme.typography.labelMedium,
                            color = MaterialTheme.colorScheme.onSurfaceVariant
                        )
                        FlowRow(
                            modifier = Modifier.fillMaxWidth(),
                            horizontalArrangement = Arrangement.spacedBy(6.dp),
                            verticalArrangement = Arrangement.spacedBy(4.dp)
                        ) {
                            listOf(
                                0 to "Stereo",
                                1 to "Left Speaker",
                                2 to "Right Speaker",
                                3 to "Center Vocal",
                                4 to "Rear Surround"
                            ).forEach { (roleIdx, roleLabel) ->
                                FilterChip(
                                    selected = channelRole == roleIdx,
                                    onClick = {
                                        channelRole = roleIdx
                                        syncAndSaveDsp()
                                    },
                                    label = { Text(roleLabel) }
                                )
                            }
                        }
                    }
                }

                // =====================================================================
                // 4. PRO DRAWER 2: Host Quality Profile, Codec Engine & Bitrate
                // =====================================================================
                val modeSummary = when (qualityMode) {
                    1 -> "16-Bit"
                    2 -> "VBR ${compressedBitrateKbps}k"
                    3 -> "10ms 2.4G"
                    4 -> "24-Bit"
                    else -> "Smart Auto"
                }
                val engineSummary = if (useOpus) "Opus RLD" else "Rust VBR"
                ProDrawerHeader(
                    title = "Host Quality, Codec Engine & Bitrate",
                    badge = "$modeSummary • $engineSummary",
                    expanded = showCodecDrawer,
                    onToggle = { showCodecDrawer = !showCodecDrawer }
                )

                AnimatedVisibility(visible = showCodecDrawer) {
                    StudioCard {
                        Text(
                            "Host Quality Mode",
                            style = MaterialTheme.typography.labelMedium,
                            color = MaterialTheme.colorScheme.onSurfaceVariant
                        )
                        FlowRow(
                            modifier = Modifier.fillMaxWidth(),
                            horizontalArrangement = Arrangement.spacedBy(6.dp),
                            verticalArrangement = Arrangement.spacedBy(4.dp)
                        ) {
                            listOf(
                                0 to "Smart Auto",
                                1 to "16-Bit Lossless",
                                4 to "24-Bit Master",
                                2 to "Low-Latency VBR",
                                3 to "10ms 2.4G"
                            ).forEach { (idx, label) ->
                                FilterChip(
                                    selected = qualityMode == idx,
                                    onClick = {
                                        qualityMode = idx
                                        prefs.edit().putInt("quality_mode", idx).apply()
                                        AudioEngineService.nativeSendMediaCommand(idx + 1)
                                    },
                                    label = { Text(label) }
                                )
                            }
                        }

                        Text(
                            "Compressed Codec Module",
                            style = MaterialTheme.typography.labelMedium,
                            color = MaterialTheme.colorScheme.onSurfaceVariant
                        )
                        FlowRow(
                            modifier = Modifier.fillMaxWidth(),
                            horizontalArrangement = Arrangement.spacedBy(8.dp),
                            verticalArrangement = Arrangement.spacedBy(4.dp)
                        ) {
                            FilterChip(
                                selected = !useOpus,
                                onClick = {
                                    useOpus = false
                                    prefs.edit().putBoolean("use_opus", false).apply()
                                    AudioEngineService.nativeSendMediaCommand(20)
                                },
                                label = { Text("Pure-Rust VBR (0ms)") }
                            )
                            FilterChip(
                                selected = useOpus,
                                onClick = {
                                    useOpus = true
                                    prefs.edit().putBoolean("use_opus", true).apply()
                                    AudioEngineService.nativeSendMediaCommand(21)
                                },
                                label = { Text("Opus RLD (Pure Rust)") }
                            )
                        }

                        if (qualityMode == 0 || qualityMode == 2 || qualityMode == 3) {
                            val stabilityMode = qualityMode == 3
                            val bitrateChoices =
                                if (stabilityMode) listOf(96, 160)
                                else listOf(96, 160, 256, 320)
                            val effectiveBitrate =
                                if (stabilityMode) compressedBitrateKbps.coerceIn(96, 160)
                                else compressedBitrateKbps

                            Text(
                                "Compressed Bitrate",
                                style = MaterialTheme.typography.labelMedium,
                                color = MaterialTheme.colorScheme.onSurfaceVariant
                            )
                            FlowRow(
                                modifier = Modifier.fillMaxWidth(),
                                horizontalArrangement = Arrangement.spacedBy(6.dp),
                                verticalArrangement = Arrangement.spacedBy(4.dp)
                            ) {
                                bitrateChoices.forEachIndexed { idx, kbps ->
                                    FilterChip(
                                        selected = effectiveBitrate == kbps,
                                        onClick = {
                                            compressedBitrateKbps = kbps
                                            prefs.edit().putInt("target_bitrate_kbps", kbps).apply()
                                            AudioEngineService.nativeSendMediaCommand(30 + idx)
                                        },
                                        label = { Text("$kbps kbps") }
                                    )
                                }
                            }
                            if (stabilityMode) {
                                Text(
                                    "2.4G stability caps the target at 160 kbps and uses 96 kbps above 200‰ loss.",
                                    style = MaterialTheme.typography.bodySmall,
                                    color = MaterialTheme.colorScheme.onSurfaceVariant
                                )
                            }
                        }
                    }
                }
            } // End Tab 1

            if (selectedTab == 2) {
                StudioCard {
                    Text(
                        "Wireless Microphone & Party Broadcast",
                        style = MaterialTheme.typography.titleMedium,
                        fontWeight = FontWeight.SemiBold
                    )
                    Text(
                        "Use your phone as a 48 kHz wireless microphone for Zoom/Discord/Meet on PC (Port 65532), or broadcast internal phone audio across multiple phones and PCs ('Android-Party-Host').",
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant
                    )

                    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
                        SettingSwitchRow(
                            title = "Cast Phone Internal Audio (Party Mode)",
                            subtitle = "Broadcasts media & games from this phone to PC speakers or peer phones",
                            checked = castPhoneInternalAudio,
                            badgeText = if (castPhoneInternalAudio) "ON AIR" else null,
                            onCheckedChange = { enabled ->
                                if (enabled) {
                                    val hasPerm = ContextCompat.checkSelfPermission(
                                        context,
                                        Manifest.permission.RECORD_AUDIO
                                    ) == PackageManager.PERMISSION_GRANTED
                                    if (hasPerm) {
                                        projectionLauncher.launch(createAudioCaptureProjectionIntent())
                                    } else {
                                        pendingInternalAudioCast = true
                                        castAudioPermissionLauncher.launch(Manifest.permission.RECORD_AUDIO)
                                    }
                                } else {
                                    castPhoneInternalAudio = false
                                    val stopIntent = Intent(context, AudioEngineService::class.java).apply {
                                        action = AudioEngineService.ACTION_STOP_PARTY_CAST
                                    }
                                    context.startService(stopIntent)
                                }
                            }
                        )

                        SettingSwitchRow(
                            title = "Mute This Phone's Speaker During Party Cast",
                            subtitle = "Keeps the room echo-free — use peers/PCs as synced speakers",
                            checked = muteHostSpeakerInParty,
                            onCheckedChange = { mute ->
                                muteHostSpeakerInParty = mute
                                prefs.edit().putBoolean("party_mute_local", mute).apply()
                                if (castPhoneInternalAudio) {
                                    context.startService(
                                        Intent(context, AudioEngineService::class.java).apply {
                                            action = AudioEngineService.ACTION_SET_PARTY_LOCAL_SPEAKER_MUTE
                                            putExtra("MUTE_LOCAL", mute)
                                        }
                                    )
                                }
                            }
                        )

                        SettingSwitchRow(
                            title = "Also Forward to PC Receiver (Port 65532)",
                            subtitle = "Unicast stream to $hostAddress (OFF = Pure Peer Auto-Discovery)",
                            checked = forwardPartyToPc,
                            onCheckedChange = { enabled ->
                                forwardPartyToPc = enabled
                                prefs.edit().putBoolean("party_forward_pc", enabled).apply()
                            }
                        )

                        AnimatedVisibility(visible = castPhoneInternalAudio) {
                            PartyTelemetrySection(
                                telemetry = { telemetry },
                                partySilenceWarning = partySilenceWarning,
                                showPartyQrDialog = showPartyQrDialog,
                                pinCode = pinCode,
                                onSilenceWarning = { partySilenceWarning = it },
                                onShowQr = { showPartyQrDialog = true },
                                onDismissQr = { showPartyQrDialog = false },
                                onCopied = {
                                    scope.launch {
                                        snackbarHostState.showSnackbar("Party link copied")
                                    }
                                }
                            )
                        }
                    }

                    SettingSwitchRow(
                        title = "Share Phone Microphone",
                        subtitle = "Streams low-latency AAudio hardware mic to PC Virtual Mic on Port 65532",
                        checked = sharePhoneMic,
                        enabled = !castPhoneInternalAudio,
                        badgeText = if (sharePhoneMic) "MIC LIVE" else null,
                        onCheckedChange = { enabled ->
                            if (!enabled) {
                                val stopIntent = Intent(context, AudioEngineService::class.java).apply {
                                    action = AudioEngineService.ACTION_STOP_PHONE_MIC
                                }
                                context.startService(stopIntent)
                            } else {
                                val hasPerm = ContextCompat.checkSelfPermission(
                                    context,
                                    Manifest.permission.RECORD_AUDIO
                                ) == PackageManager.PERMISSION_GRANTED
                                if (hasPerm) {
                                    startPhoneMicInService()
                                } else {
                                    micPermissionLauncher.launch(Manifest.permission.RECORD_AUDIO)
                                }
                            }
                        }
                    )
                }
            } // End Tab 2
        }
    }

    // Secondary Connect tools, tucked away from the primary flow.
    if (showToolsSheet) {
        ModalBottomSheet(
            onDismissRequest = { showToolsSheet = false },
            sheetState = rememberModalBottomSheetState(skipPartiallyExpanded = true),
            containerColor = StudioColors.CardBg
        ) {
            Column(
                modifier = Modifier
                    .fillMaxWidth()
                    .padding(horizontal = 20.dp, vertical = 10.dp)
                    .navigationBarsPadding()
                    .verticalScroll(rememberScrollState()),
                verticalArrangement = Arrangement.spacedBy(12.dp)
            ) {
                SectionEyebrow("Remote PC Playback")
                Row(
                    modifier = Modifier.fillMaxWidth(),
                    horizontalArrangement = Arrangement.spacedBy(8.dp)
                ) {
                    OutlinedButton(
                        onClick = { AudioEngineService.nativeSendMediaCommand(12) },
                        modifier = Modifier.weight(1f)
                    ) {
                        Icon(Icons.Rounded.SkipPrevious, contentDescription = "Previous")
                    }
                    FilledTonalButton(
                        onClick = { AudioEngineService.nativeSendMediaCommand(10) },
                        modifier = Modifier.weight(1.4f)
                    ) {
                        Icon(Icons.Rounded.PlayArrow, contentDescription = "Play / Pause")
                        Spacer(Modifier.width(4.dp))
                        Text("Play / Pause")
                    }
                    OutlinedButton(
                        onClick = { AudioEngineService.nativeSendMediaCommand(11) },
                        modifier = Modifier.weight(1f)
                    ) {
                        Icon(Icons.Rounded.SkipNext, contentDescription = "Next")
                    }
                }
                SectionEyebrow("Sleep Timer")
                FlowRow(horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                    listOf(0 to "Off", 15 to "15m", 30 to "30m", 60 to "60m").forEach { (mins, lbl) ->
                        FilterChip(
                            selected = when (mins) {
                                0 -> sleepTimerMinutes == 0
                                15 -> sleepTimerMinutes in 1..15
                                30 -> sleepTimerMinutes in 16..30
                                else -> sleepTimerMinutes > 30
                            },
                            onClick = {
                                runCatching {
                                    context.startService(
                                        Intent(context, AudioEngineService::class.java).apply {
                                            action = AudioEngineService.ACTION_SET_SLEEP_TIMER
                                            putExtra("MINUTES", mins)
                                        }
                                    )
                                }
                            },
                            label = { Text(lbl) }
                        )
                    }
                }
                SectionEyebrow("Manual Host IP & PIN")
                OutlinedTextField(
                    value = hostAddress,
                    onValueChange = { hostAddress = it },
                    label = { Text("Host IP:Port") },
                    singleLine = true,
                    modifier = Modifier.fillMaxWidth()
                )
                OutlinedTextField(
                    value = pinCode,
                    onValueChange = {
                        pinCode = it
                        prefs.edit().putString("pin_code", it).apply()
                    },
                    label = { Text("PIN (Optional, ChaCha20-Poly1305 AEAD)") },
                    singleLine = true,
                    modifier = Modifier.fillMaxWidth()
                )
                DiagnosticsActionsRow(
                    snackbarHostState = snackbarHostState,
                    onDone = { showToolsSheet = false }
                )
                Spacer(Modifier.height(12.dp))
            }
        }
    }

    if (showQrScanner) {
        QrScannerDialog(
            onDismiss = { showQrScanner = false },
            onScanned = { host, pin ->
                showQrScanner = false
                connectToHost(host, false, pin)
            }
        )
    }

    // Tapping an encrypted host prompts for its PIN in one gesture.
    pendingEncryptedHost?.let { entry ->
        val parts = entry.split("|")
        val encAddr = parts.getOrNull(1) ?: entry
        AlertDialog(
            onDismissRequest = { pendingEncryptedHost = null },
            title = { Text("Encrypted Host") },
            text = {
                Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                    Text("Enter the pairing PIN for $encAddr")
                    OutlinedTextField(
                        value = promptPinInput,
                        onValueChange = { promptPinInput = it },
                        label = { Text("PIN") },
                        singleLine = true,
                        modifier = Modifier.fillMaxWidth()
                    )
                }
            },
            confirmButton = {
                TextButton(onClick = {
                    val host = pendingEncryptedHost ?: return@TextButton
                    val p = host.split("|")
                    val addr = p.getOrNull(1) ?: host
                    val isUsb = p.getOrNull(2).equals("USB", ignoreCase = true)
                    pendingEncryptedHost = null
                    if (promptPinInput.isBlank()) {
                        scope.launch {
                            snackbarHostState.showSnackbar("PIN is required for this host")
                        }
                    } else {
                        connectToHost(addr, isUsb, promptPinInput)
                    }
                }) { Text("Connect") }
            },
            dismissButton = {
                TextButton(onClick = { pendingEncryptedHost = null }) { Text("Cancel") }
            }
        )
    }
}
