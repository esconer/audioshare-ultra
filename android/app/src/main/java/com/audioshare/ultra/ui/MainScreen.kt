package com.audioshare.ultra.ui

import android.Manifest
import android.app.Activity
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.content.pm.PackageManager
import android.media.AudioAttributes
import android.media.AudioDeviceInfo
import android.media.AudioManager
import android.media.projection.MediaProjectionConfig
import android.media.projection.MediaProjectionManager
import android.net.Uri
import android.os.Build
import android.os.PowerManager
import android.provider.MediaStore
import android.provider.Settings
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.animation.AnimatedVisibility
import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.clickable
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.core.content.ContextCompat
import com.audioshare.ultra.service.AudioEngineService
import kotlinx.coroutines.delay
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

@Composable
private fun StudioCard(
    modifier: Modifier = Modifier,
    content: @Composable ColumnScope.() -> Unit
) {
    OutlinedCard(
        modifier = modifier.fillMaxWidth(),
        shape = RoundedCornerShape(14.dp),
        colors = CardDefaults.outlinedCardColors(
            containerColor = MaterialTheme.colorScheme.surfaceVariant.copy(alpha = 0.78f)
        ),
        border = BorderStroke(1.dp, Color(0xFF243047))
    ) {
        Column(
            modifier = Modifier.padding(16.dp),
            verticalArrangement = Arrangement.spacedBy(10.dp),
            content = content
        )
    }
}

@Composable
private fun StatusBadge(
    text: String,
    containerColor: Color = Color(0xFF1A2436),
    contentColor: Color = Color(0xFFCBD5E1),
    onClick: (() -> Unit)? = null
) {
    Surface(
        shape = RoundedCornerShape(8.dp),
        color = containerColor,
        contentColor = contentColor,
        border = BorderStroke(1.dp, Color(0xFF2B3852)),
        modifier = if (onClick != null) Modifier.clickable { onClick() } else Modifier
    ) {
        Text(
            text = text,
            style = MaterialTheme.typography.labelSmall,
            fontFamily = FontFamily.Monospace,
            fontWeight = FontWeight.SemiBold,
            modifier = Modifier.padding(horizontal = 8.dp, vertical = 4.dp)
        )
    }
}

@Composable
private fun MetricTile(label: String, value: String, modifier: Modifier = Modifier) {
    Surface(
        modifier = modifier,
        shape = RoundedCornerShape(10.dp),
        color = Color(0xFF0E131D),
        border = BorderStroke(1.dp, Color(0xFF202A3E))
    ) {
        Column(modifier = Modifier.padding(horizontal = 10.dp, vertical = 8.dp)) {
            Text(
                text = label,
                style = MaterialTheme.typography.labelSmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant
            )
            Text(
                text = value,
                style = MaterialTheme.typography.bodyMedium,
                fontFamily = FontFamily.Monospace,
                fontWeight = FontWeight.SemiBold
            )
        }
    }
}

@Composable
private fun SettingSwitchRow(
    title: String,
    subtitle: String,
    checked: Boolean,
    enabled: Boolean = true,
    badgeText: String? = null,
    onCheckedChange: (Boolean) -> Unit
) {
    Surface(
        shape = RoundedCornerShape(10.dp),
        color = Color(0xFF0E131D),
        border = BorderStroke(1.dp, if (checked) Color(0xFF2563EB) else Color(0xFF202A3E)),
        modifier = Modifier.fillMaxWidth()
    ) {
        Row(
            modifier = Modifier
                .fillMaxWidth()
                .padding(horizontal = 12.dp, vertical = 10.dp),
            horizontalArrangement = Arrangement.SpaceBetween,
            verticalAlignment = Alignment.CenterVertically
        ) {
            Column(
                modifier = Modifier
                    .weight(1f)
                    .padding(end = 10.dp),
                verticalArrangement = Arrangement.spacedBy(2.dp)
            ) {
                Row(
                    verticalAlignment = Alignment.CenterVertically,
                    horizontalArrangement = Arrangement.spacedBy(8.dp)
                ) {
                    Text(
                        text = title,
                        style = MaterialTheme.typography.bodyMedium,
                        fontWeight = FontWeight.SemiBold
                    )
                    if (badgeText != null) {
                        StatusBadge(
                            text = badgeText,
                            containerColor = Color(0xFF14532D),
                            contentColor = Color(0xFF86EFAC)
                        )
                    }
                }
                Text(
                    text = subtitle,
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant
                )
            }
            Switch(
                checked = checked,
                enabled = enabled,
                onCheckedChange = onCheckedChange
            )
        }
    }
}

@Composable
private fun ProDrawerHeader(
    title: String,
    badge: String,
    expanded: Boolean,
    onToggle: () -> Unit
) {
    Surface(
        shape = RoundedCornerShape(12.dp),
        color = if (expanded) Color(0xFF172238) else Color(0xFF121826),
        border = BorderStroke(1.dp, if (expanded) Color(0xFF3B82F6) else Color(0xFF243047)),
        modifier = Modifier
            .fillMaxWidth()
            .clickable { onToggle() }
    ) {
        Row(
            modifier = Modifier
                .fillMaxWidth()
                .padding(horizontal = 14.dp, vertical = 12.dp),
            horizontalArrangement = Arrangement.SpaceBetween,
            verticalAlignment = Alignment.CenterVertically
        ) {
            Text(
                text = "${if (expanded) "▾" else "▸"}  $title",
                style = MaterialTheme.typography.titleSmall,
                fontWeight = FontWeight.SemiBold,
                modifier = Modifier.weight(1f)
            )
            StatusBadge(
                text = badge,
                containerColor = Color(0xFF1E293B),
                contentColor = Color(0xFF93C5FD)
            )
        }
    }
}

@Composable
private fun StudioFaderRow(
    title: String,
    freqLabel: String,
    value: Float,
    valueRange: ClosedFloatingPointRange<Float>,
    onValueChange: (Float) -> Unit
) {
    Column(verticalArrangement = Arrangement.spacedBy(2.dp)) {
        Row(
            modifier = Modifier.fillMaxWidth(),
            horizontalArrangement = Arrangement.SpaceBetween,
            verticalAlignment = Alignment.CenterVertically
        ) {
            Row(
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.spacedBy(6.dp)
            ) {
                Text(
                    text = title,
                    style = MaterialTheme.typography.bodyMedium,
                    fontWeight = FontWeight.Medium
                )
                if (freqLabel.isNotEmpty()) {
                    StatusBadge(text = freqLabel)
                }
            }
            val boosted = abs(value) >= 0.05f
            StatusBadge(
                text = "${"%+.1f".format(value)} dB",
                containerColor = if (boosted) Color(0xFF1E3A5F) else Color(0xFF1A2436),
                contentColor = if (boosted) Color(0xFF93C5FD) else Color(0xFF94A3B8),
                onClick = { onValueChange(0f) }
            )
        }
        Slider(
            value = value,
            onValueChange = onValueChange,
            valueRange = valueRange
        )
    }
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun MainScreen() {
    val context = LocalContext.current
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

    var hostAddress by remember {
        mutableStateOf(prefs.getString("last_host_addr", "192.168.42.129:65530") ?: "192.168.42.129:65530")
    }
    var pinCode by remember {
        mutableStateOf(prefs.getString("pin_code", "") ?: "")
    }
    var selectedTab by remember { mutableIntStateOf(0) } // 0=Connect, 1=Audio & Tune, 2=Mic & Party
    var isConnected by remember { mutableStateOf(AudioEngineService.isStreaming) }
    var preampGainDb by remember { mutableFloatStateOf(prefs.getFloat("gain_db", 0.0f)) }
    var bassDb by remember { mutableFloatStateOf(prefs.getFloat("bass_db", 0.0f)) }
    var vocalDb by remember { mutableFloatStateOf(prefs.getFloat("vocal_db", 0.0f)) }
    var trebleDb by remember { mutableFloatStateOf(prefs.getFloat("treble_db", 0.0f)) }
    var nightMode by remember { mutableStateOf(prefs.getBoolean("night_mode", false)) }
    var crossfeed by remember { mutableStateOf(prefs.getBoolean("crossfeed", false)) }
    var gamingLock by remember { mutableStateOf(prefs.getBoolean("gaming_lock", false)) }
    // OFF (default) keeps the raw low-latency AAudio path. ON switches to the
    // shared mixer so OEM Dolby Atmos / Dirac / system EQ can post-process.
    var dolbyEffects by remember { mutableStateOf(prefs.getBoolean("dolby_fx", false)) }
    var pendingInternalAudioCast by remember { mutableStateOf(false) }
    var latencyDebug by remember { mutableStateOf(prefs.getBoolean("latency_debug", false)) }
    var channelRole by remember { mutableIntStateOf(prefs.getInt("channel_role", 0)) }
    // 0=Smart Auto, 1=Studio Lossless 16-bit, 2=Low-Latency VBR,
    // 3=Max Stability (10ms 2.4GHz), 4=Studio Master 24-bit
    var qualityMode by remember { mutableIntStateOf(prefs.getInt("quality_mode", 0)) }
    // Target for the compressed profiles. Persisted and restored on the next
    // stream start; command codes 30..33 are 96/160/256/320 kbps.
    var compressedBitrateKbps by remember {
        mutableIntStateOf(
            prefs.getInt("target_bitrate_kbps", 256).let {
                if (it == 96 || it == 160 || it == 256 || it == 320) it else 256
            }
        )
    }
    // false = pure-Rust VBR (default), true = bundled pure-Rust Opus RLD.
    // Sent to the host as feedback code 20/21; only affects compressed modes.
    var useOpus by remember { mutableStateOf(prefs.getBoolean("use_opus", false)) }
    var sharePhoneMic by remember { mutableStateOf(false) }
    // The service owns the MediaProjection token and the caster. The UI only
    // mirrors the service's broadcast so a system-revoked projection turns
    // the switch off even if the app was backgrounded.
    var castPhoneInternalAudio by remember { mutableStateOf(AudioEngineService.isPartyCasting) }
    var sleepTimerMinutes by remember { mutableIntStateOf(0) } // 0=Off, 15, 30, 60m
    // Index contract matches crates/android-jni/src/lib.rs:
    // 0..10 legacy counters, 11..21 live latency breakdown.
    var telemetry by remember { mutableStateOf(LongArray(24)) }
    var btEarbudsDetected by remember { mutableStateOf(false) }

    // Progressive Disclosure Drawer States ("Simple First, Complex When Needed")
    var showManualConnect by remember { mutableStateOf(false) }
    var showTelemetryDrawer by remember { mutableStateOf(latencyDebug) }
    var showEqDrawer by remember { mutableStateOf(false) }
    var showCodecDrawer by remember { mutableStateOf(false) }

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
        AudioEngineService.nativeSetGainDb(preampGainDb)
        AudioEngineService.nativeConfigureDsp(
            bassDb, vocalDb, trebleDb, channelRole,
            crossfeed, nightMode, gamingLock, dolbyEffects
        )
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
                    castPhoneInternalAudio = intent.getBooleanExtra(
                        AudioEngineService.EXTRA_PARTY_CAST_ACTIVE,
                        false
                    )
                }
            }
        }
        val filter = IntentFilter(AudioEngineService.ACTION_PARTY_CAST_STATE)
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

    // The foreground service must hold the MICROPHONE type before a hardware
    // mic capture starts, especially with targetSdk 34 and the screen off.
    // Plain playback and playback capture never call this.
    fun enterCaptureForeground(label: String) {
        if (isConnected) {
            val intent = Intent(context, AudioEngineService::class.java).apply {
                action = "ACTION_SET_MIC_CAPTURE"
                putExtra("MIC_ACTIVE", true)
            }
            context.startService(intent)
        } else {
            val guardIntent = Intent(context, AudioEngineService::class.java).apply {
                action = "ACTION_START_CAST_GUARD"
                putExtra("CAST_LABEL", label)
            }
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                context.startForegroundService(guardIntent)
            } else {
                context.startService(guardIntent)
            }
        }
    }

    fun exitCaptureForeground(remainingCapture: Boolean) {
        if (isConnected) {
            val intent = Intent(context, AudioEngineService::class.java).apply {
                action = "ACTION_SET_MIC_CAPTURE"
                putExtra("MIC_ACTIVE", remainingCapture)
            }
            context.startService(intent)
        } else if (!remainingCapture) {
            val stopIntent = Intent(context, AudioEngineService::class.java).apply {
                action = "ACTION_STOP"
            }
            context.startService(stopIntent)
        }
    }

    val micPermissionLauncher = rememberLauncherForActivityResult(
        contract = ActivityResultContracts.RequestPermission()
    ) { granted ->
        if (granted) {
            sharePhoneMic = true
            enterCaptureForeground("PC Video-Call Mic Active")
            AudioEngineService.nativeStartPhoneMic(hostAddress, true)
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
            // The native phone-mic transmitter is a single global. Release
            // the hardware mic before the service hands it to the caster.
            if (sharePhoneMic) {
                sharePhoneMic = false
                AudioEngineService.nativeStopPhoneMic()
            }
            val svcIntent = Intent(context, AudioEngineService::class.java).apply {
                action = AudioEngineService.ACTION_START_PARTY_CAST
                putExtra("PROJECTION_RESULT_CODE", result.resultCode)
                putExtra("PROJECTION_DATA", resultData)
                putExtra("TARGET_HOSTS", hostAddress)
            }
            try {
                if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                    context.startForegroundService(svcIntent)
                } else {
                    context.startService(svcIntent)
                }
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

    // Sleep Timer Countdown Coroutine (Auto-stops stream after 15/30/60 minutes)
    LaunchedEffect(sleepTimerMinutes, isConnected) {
        if (sleepTimerMinutes > 0 && isConnected) {
            delay(sleepTimerMinutes * 60_000L)
            val stopIntent = Intent(context, AudioEngineService::class.java).apply {
                action = "ACTION_STOP"
            }
            context.startService(stopIntent)
            isConnected = false
            sleepTimerMinutes = 0
        }
    }

    // 1-Tap auto-discovery: the native UDP beacon scanner runs in the Rust JNI layer
    var discoveredHosts by remember { mutableStateOf<List<String>>(emptyList()) }
    LaunchedEffect(Unit) {
        while (true) {
            discoveredHosts = runCatching {
                AudioEngineService.nativeGetDiscoveredHosts()
            }.getOrDefault("").lines().filter { it.isNotBlank() }
            delay(2000)
        }
    }

    // Poll 4Hz native telemetry & check if Bluetooth A2DP headphones are connected
    LaunchedEffect(isConnected, dolbyEffects) {
        val audioManager = context.getSystemService(Context.AUDIO_SERVICE) as AudioManager
        while (isConnected) {
            telemetry = AudioEngineService.nativeGetTelemetry()

            // Auto-disconnect if PC host has been silent/unreachable for 5 minutes (300 seconds)
            val silenceSec = telemetry.getOrElse(6) { 0L }
            if (silenceSec >= 300L) {
                val stopIntent = Intent(context, AudioEngineService::class.java).apply {
                    action = "ACTION_STOP"
                }
                context.startService(stopIntent)
                isConnected = false
                break
            }

            // Only treat Bluetooth as active when media audio is actually routed there.
            val hasBt = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
                val attrs = AudioAttributes.Builder().apply {
                    if (dolbyEffects) {
                        setUsage(AudioAttributes.USAGE_MEDIA)
                        setContentType(AudioAttributes.CONTENT_TYPE_MUSIC)
                    } else {
                        setUsage(AudioAttributes.USAGE_GAME)
                        setContentType(AudioAttributes.CONTENT_TYPE_SONIFICATION)
                    }
                }.build()
                val routedType = audioManager.getAudioDevicesForAttributes(attrs)
                    .firstOrNull()?.type
                routedType == AudioDeviceInfo.TYPE_BLUETOOTH_A2DP ||
                    routedType == AudioDeviceInfo.TYPE_BLUETOOTH_SCO ||
                    routedType == AudioDeviceInfo.TYPE_BLE_HEADSET ||
                    routedType == AudioDeviceInfo.TYPE_BLE_SPEAKER
            } else {
                @Suppress("DEPRECATION")
                val a2dpRouted = audioManager.isBluetoothA2dpOn
                @Suppress("DEPRECATION")
                val scoRouted = audioManager.isBluetoothScoOn()
                a2dpRouted || scoRouted
            }
            btEarbudsDetected = hasBt
            AudioEngineService.nativeReportBluetoothDelayMs(if (hasBt) 145 else 0)
            delay(250)
        }
    }

    Scaffold(
        topBar = {
            TopAppBar(
                title = {
                    Row(
                        verticalAlignment = Alignment.CenterVertically,
                        horizontalArrangement = Arrangement.spacedBy(8.dp)
                    ) {
                        Text("AudioShare Ultra", fontWeight = FontWeight.Bold)
                        StatusBadge(
                            text = if (isConnected) "• LIVE" else "• STANDBY",
                            containerColor = if (isConnected) Color(0xFF14532D) else Color(0xFF1A2436),
                            contentColor = if (isConnected) Color(0xFF86EFAC) else Color(0xFF94A3B8)
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
            // PERSISTENT HERO STATUS BANNER
            // =====================================================================
            StudioCard {
                val authErr = telemetry.getOrElse(5) { 0L }
                val idleSecs = telemetry.getOrElse(6) { 0L }
                val dtxSilent = telemetry.getOrElse(7) { 0L } == 1L
                val hostVol = telemetry.getOrElse(8) { 100L }
                val groupDelay = telemetry.getOrElse(9) { 0L }
                val codecMode = telemetry.getOrElse(19) { 0L }

                val statusTitle = when {
                    !isConnected -> "⚪ Ready to Connect — Select a host below or scan QR"
                    authErr > 5 -> "🔴 Wrong Encryption PIN! ($authErr packets rejected)"
                    idleSecs >= 3 -> "⚠️ Host Signal Lost (${idleSecs}s) — Reconnecting..."
                    dtxSilent -> "🔋 Smart DTX Standby (-90 dBFS BT Keep-Alive)"
                    else -> "🟢 Streaming 48 kHz • ${codecModeName(codecMode)}"
                }
                Row(
                    modifier = Modifier.fillMaxWidth(),
                    horizontalArrangement = Arrangement.SpaceBetween,
                    verticalAlignment = Alignment.CenterVertically
                ) {
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
                }
                if (isConnected) {
                    Row(
                        modifier = Modifier
                            .fillMaxWidth()
                            .horizontalScroll(rememberScrollState()),
                        horizontalArrangement = Arrangement.spacedBy(6.dp)
                    ) {
                        StatusBadge("Pkts: ${telemetry[0]}")
                        StatusBadge("FEC: ${telemetry[2]}")
                        StatusBadge("Drift: ${telemetry[4]} ppm")
                        if (groupDelay > 0) {
                            StatusBadge("Sync: +${groupDelay}ms")
                        }
                        if (latencyDebug) {
                            val estMs = telemetry.getOrElse(20) { 0L } / 10.0
                            val targetMs = telemetry.getOrElse(14) { 0L } / 1000.0
                            val bufferedMs = telemetry.getOrElse(15) { 0L } / 1000.0
                            StatusBadge(
                                text = "Est ${"%.1f".format(estMs)}ms • FIFO ${"%.1f".format(bufferedMs)}/${"%.1f".format(targetMs)}ms",
                                containerColor = Color(0xFF14532D),
                                contentColor = Color(0xFF86EFAC)
                            )
                        }
                    }
                }
            }

            // 3-Tab Material 3 Navigation Bar
            TabRow(
                selectedTabIndex = selectedTab,
                containerColor = MaterialTheme.colorScheme.surface,
                contentColor = MaterialTheme.colorScheme.primary
            ) {
                Tab(selected = selectedTab == 0, onClick = { selectedTab = 0 }, text = { Text("Connect") })
                Tab(selected = selectedTab == 1, onClick = { selectedTab = 1 }, text = { Text("Audio & Tune") })
                Tab(selected = selectedTab == 2, onClick = { selectedTab = 2 }, text = { Text("Mic & Party") })
            }

            if (selectedTab == 0) {
                // 0. 1-Tap OEM Battery / Doze Exemption Banner
                if (!isBatteryUnrestricted && Build.VERSION.SDK_INT >= Build.VERSION_CODES.M) {
                    StudioCard {
                        Row(
                            modifier = Modifier.fillMaxWidth(),
                            horizontalArrangement = Arrangement.SpaceBetween,
                            verticalAlignment = Alignment.CenterVertically
                        ) {
                            Text(
                                "⚡ Disable Battery Throttling for 0-Jitter Screen-Off Audio",
                                modifier = Modifier.weight(1f),
                                style = MaterialTheme.typography.bodySmall
                            )
                            OutlinedButton(onClick = {
                                try {
                                    val i = Intent(Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS).apply {
                                        data = Uri.parse("package:${context.packageName}")
                                    }
                                    context.startActivity(i)
                                    isBatteryUnrestricted = true
                                } catch (_: Exception) {}
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
                            val kind = if (name.contains("Party")) "Party Phone" else "PC"
                            FilledTonalButton(
                                onClick = {
                                    hostAddress = addr
                                    prefs.edit()
                                        .putString("last_host_addr", hostAddress)
                                        .putString("pin_code", pinCode)
                                        .apply()
                                    val intent = Intent(context, AudioEngineService::class.java).apply {
                                        putExtra("HOST_ADDR", hostAddress)
                                        putExtra("IS_USB", mode.equals("USB", ignoreCase = true))
                                        putExtra("GAIN_DB", preampGainDb)
                                        putExtra("PIN", pinCode)
                                    }
                                    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                                        context.startForegroundService(intent)
                                    } else {
                                        context.startService(intent)
                                    }
                                    isConnected = true
                                },
                                modifier = Modifier.fillMaxWidth()
                            ) {
                                Text("▶  [$mode] $kind: $name ($addr)")
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
                            prefs.edit()
                                .putString("last_host_addr", hostAddress)
                                .putString("pin_code", pinCode)
                                .apply()
                            val intent = Intent(context, AudioEngineService::class.java)
                            if (isConnected) {
                                intent.action = "ACTION_STOP"
                                context.startService(intent)
                                isConnected = false
                            } else {
                                intent.putExtra("HOST_ADDR", hostAddress)
                                intent.putExtra("GAIN_DB", preampGainDb)
                                intent.putExtra("PIN", pinCode)
                                if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                                    context.startForegroundService(intent)
                                } else {
                                    context.startService(intent)
                                }
                                isConnected = true
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
                        Text(
                            if (isConnected) "■  Disconnect Stream" else "▶  Connect Lossless Stream",
                            fontWeight = FontWeight.Bold
                        )
                    }

                    Row(
                        modifier = Modifier.fillMaxWidth(),
                        horizontalArrangement = Arrangement.spacedBy(8.dp)
                    ) {
                        OutlinedButton(
                            onClick = { hostAddress = "127.0.0.1:65530" },
                            modifier = Modifier.weight(1f)
                        ) {
                            Text("USB ADB")
                        }
                        OutlinedButton(
                            onClick = {
                                try {
                                    val qrIntent = Intent("com.google.zxing.client.android.SCAN")
                                    context.startActivity(qrIntent)
                                } catch (_: Exception) {
                                    val camIntent = Intent(MediaStore.INTENT_ACTION_STILL_IMAGE_CAMERA)
                                    context.startActivity(camIntent)
                                }
                            },
                            modifier = Modifier.weight(1f)
                        ) {
                            Text("Scan QR")
                        }
                        OutlinedButton(
                            onClick = { showManualConnect = !showManualConnect },
                            modifier = Modifier.weight(1f)
                        ) {
                            Text(if (showManualConnect) "Hide IP" else "IP / PIN")
                        }
                    }

                    // Progressive Disclosure: Manual Host IP & PIN fields.
                    // Keep the PIN editor visible while no PIN is saved, even
                    // when a discovered host is available, so a first-time
                    // encrypted connection cannot fail on a hidden credential.
                    AnimatedVisibility(
                        visible = showManualConnect || discoveredHosts.isEmpty() || pinCode.isBlank()
                    ) {
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
                                label = { Text("PIN (Optional, ChaCha20 + MAC32)") },
                                singleLine = true,
                                modifier = Modifier.fillMaxWidth()
                            )
                        }
                    }
                }

                // =====================================================================
                // 2. QUICK PC MEDIA TRANSPORT & SLEEP TIMER CARD
                // =====================================================================
                StudioCard {
                    Text(
                        "Remote PC Media & Sleep Timer",
                        style = MaterialTheme.typography.titleSmall,
                        fontWeight = FontWeight.SemiBold
                    )
                    Row(
                        modifier = Modifier.fillMaxWidth(),
                        horizontalArrangement = Arrangement.spacedBy(8.dp)
                    ) {
                        OutlinedButton(
                            onClick = { AudioEngineService.nativeSendMediaCommand(12) },
                            modifier = Modifier.weight(1f)
                        ) { Text("⏮ Prev") }
                        FilledTonalButton(
                            onClick = { AudioEngineService.nativeSendMediaCommand(10) },
                            modifier = Modifier.weight(1.4f)
                        ) { Text("⏯ Play / Pause") }
                        OutlinedButton(
                            onClick = { AudioEngineService.nativeSendMediaCommand(11) },
                            modifier = Modifier.weight(1f)
                        ) { Text("⏭ Next") }
                    }

                    Row(
                        modifier = Modifier.fillMaxWidth(),
                        horizontalArrangement = Arrangement.SpaceBetween,
                        verticalAlignment = Alignment.CenterVertically
                    ) {
                        Text("Sleep Timer:", style = MaterialTheme.typography.labelMedium)
                        Row(horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                            listOf(0 to "Off", 15 to "15m", 30 to "30m", 60 to "60m").forEach { (mins, lbl) ->
                                FilterChip(
                                    selected = sleepTimerMinutes == mins,
                                    onClick = { sleepTimerMinutes = mins },
                                    label = { Text(lbl) }
                                )
                            }
                        }
                    }
                }

                // =====================================================================
                // 3. PRO TELEMETRY & STAGE-BY-STAGE LATENCY DRAWER (On-Demand)
                // =====================================================================
                ProDrawerHeader(
                    title = "Real-Time Telemetry & Latency Breakdown",
                    badge = "Drift ${telemetry[4]} ppm • FEC ${telemetry[2]}",
                    expanded = showTelemetryDrawer,
                    onToggle = { showTelemetryDrawer = !showTelemetryDrawer }
                )

                AnimatedVisibility(visible = showTelemetryDrawer) {
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
                                onClick = {
                                    val next = !latencyDebug
                                    latencyDebug = next
                                    prefs.edit().putBoolean("latency_debug", next).apply()
                                },
                                label = { Text("Stage Breakdown") }
                            )
                        }

                        Row(modifier = Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                            MetricTile("RX Packets", "${telemetry[0]}", Modifier.weight(1f))
                            MetricTile("Lossless Frames", "${telemetry[1]}", Modifier.weight(1f))
                        }
                        Row(modifier = Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                            MetricTile("FEC Healed", "${telemetry[2]}", Modifier.weight(1f))
                            MetricTile("WSOLA PLC", "${telemetry[3]}", Modifier.weight(1f))
                        }
                        Row(modifier = Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                            MetricTile("Clock Drift", "${telemetry[4]} ppm", Modifier.weight(1f))
                            MetricTile("AAudio DAC", "${telemetry.getOrElse(10) { 0L }} ms", Modifier.weight(1f))
                        }
                        Row(modifier = Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                            MetricTile("Host Silence", "${telemetry.getOrElse(6) { 0L }} s", Modifier.weight(1f))
                            MetricTile("Auth Errors", "${telemetry.getOrElse(5) { 0L }}", Modifier.weight(1f))
                        }

                        if (btEarbudsDetected) {
                            StatusBadge(
                                text = "Bluetooth Earbuds Active: Reporting +145ms A/V Sync Offset to PC",
                                containerColor = Color(0xFF14532D),
                                contentColor = Color(0xFF86EFAC)
                            )
                        }

                        if (latencyDebug) {
                            val packetUs = telemetry.getOrElse(11) { 0L }
                            val jitterUs = telemetry.getOrElse(12) { 0L }
                            val adaptiveUs = telemetry.getOrElse(13) { 0L }
                            val targetUs = telemetry.getOrElse(14) { 0L }
                            val bufferedUs = telemetry.getOrElse(15) { 0L }
                            val groupUs = telemetry.getOrElse(16) { 0L }
                            val btMs = telemetry.getOrElse(17) { 0L }
                            val prebufferReady = telemetry.getOrElse(18) { 0L } == 1L
                            val codecMode = telemetry.getOrElse(19) { 0L }
                            val estMs = telemetry.getOrElse(20) { 0L } / 10.0
                            val codecLookaheadMs = telemetry.getOrElse(21) { 0L } / 10.0
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
                            Text("DAC queue          : ${telemetry.getOrElse(10) { 0L }} ms", fontFamily = FontFamily.Monospace, style = MaterialTheme.typography.bodySmall)
                            Text("Codec lookahead    : ${"%.1f".format(codecLookaheadMs)} ms", fontFamily = FontFamily.Monospace, style = MaterialTheme.typography.bodySmall)
                            Text("Bluetooth estimate : $btLabel", fontFamily = FontFamily.Monospace, style = MaterialTheme.typography.bodySmall)
                            Text("Prebuffer / Codec  : $prebufferLabel • ${codecModeName(codecMode)}", fontFamily = FontFamily.Monospace, style = MaterialTheme.typography.bodySmall)
                            Text(
                                "Estimate = packetization + target FIFO + max(DAC, BT, output) + codec lookahead.",
                                style = MaterialTheme.typography.bodySmall,
                                color = MaterialTheme.colorScheme.onSurfaceVariant
                            )
                        }
                    }
                }
            } // End Tab 0

            if (selectedTab == 1) {
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

                    Row(
                        modifier = Modifier
                            .fillMaxWidth()
                            .horizontalScroll(rememberScrollState()),
                        horizontalArrangement = Arrangement.spacedBy(8.dp)
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
                            syncAndSaveDsp()
                        }
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
                                syncAndSaveDsp()
                            }
                        )
                        StudioFaderRow(
                            title = "Vocal Presence",
                            freqLabel = "2.5 kHz",
                            value = vocalDb,
                            valueRange = -12.0f..12.0f,
                            onValueChange = {
                                vocalDb = it
                                syncAndSaveDsp()
                            }
                        )
                        StudioFaderRow(
                            title = "Treble Air",
                            freqLabel = "10 kHz",
                            value = trebleDb,
                            valueRange = -12.0f..12.0f,
                            onValueChange = {
                                trebleDb = it
                                syncAndSaveDsp()
                            }
                        )

                        HorizontalDivider(color = Color(0xFF243047))
                        Text(
                            "Spatial Speaker Role",
                            style = MaterialTheme.typography.labelMedium,
                            color = MaterialTheme.colorScheme.onSurfaceVariant
                        )
                        Row(
                            modifier = Modifier
                                .fillMaxWidth()
                                .horizontalScroll(rememberScrollState()),
                            horizontalArrangement = Arrangement.spacedBy(6.dp)
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
                        Row(
                            modifier = Modifier
                                .fillMaxWidth()
                                .horizontalScroll(rememberScrollState()),
                            horizontalArrangement = Arrangement.spacedBy(6.dp)
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
                        Row(
                            modifier = Modifier
                                .fillMaxWidth()
                                .horizontalScroll(rememberScrollState()),
                            horizontalArrangement = Arrangement.spacedBy(8.dp)
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
                            Row(
                                modifier = Modifier
                                    .fillMaxWidth()
                                    .horizontalScroll(rememberScrollState()),
                                horizontalArrangement = Arrangement.spacedBy(6.dp)
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
                    }

                    SettingSwitchRow(
                        title = "Share Phone Microphone",
                        subtitle = "Streams low-latency AAudio hardware mic to PC Virtual Mic on Port 65532",
                        checked = sharePhoneMic,
                        enabled = !castPhoneInternalAudio,
                        badgeText = if (sharePhoneMic) "MIC LIVE" else null,
                        onCheckedChange = { enabled ->
                            if (!enabled) {
                                sharePhoneMic = false
                                AudioEngineService.nativeStopPhoneMic()
                                exitCaptureForeground(castPhoneInternalAudio)
                            } else {
                                val hasPerm = ContextCompat.checkSelfPermission(
                                    context,
                                    Manifest.permission.RECORD_AUDIO
                                ) == PackageManager.PERMISSION_GRANTED
                                if (hasPerm) {
                                    sharePhoneMic = true
                                    enterCaptureForeground("PC Video-Call Mic Active")
                                    AudioEngineService.nativeStartPhoneMic(hostAddress, true)
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
}
