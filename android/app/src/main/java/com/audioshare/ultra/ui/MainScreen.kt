package com.audioshare.ultra.ui

import android.Manifest
import android.app.Activity
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.media.AudioAttributes
import android.media.AudioDeviceInfo
import android.media.AudioManager
import android.media.projection.MediaProjectionManager
import android.net.Uri
import android.os.Build
import android.os.PowerManager
import android.provider.MediaStore
import android.provider.Settings
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.unit.dp
import androidx.core.content.ContextCompat
import com.audioshare.ultra.service.AudioEngineService
import com.audioshare.ultra.service.PhoneAudioCaster
import kotlinx.coroutines.delay

private fun usToMs(us: Long): String = "%.1f".format(us / 1000.0)

private fun codecModeName(mode: Long): String = when (mode.toInt()) {
    0 -> "Lossless LPC"
    1 -> "Raw PCM16"
    2 -> "VBR Compressed"
    3 -> "DTX Silence"
    4 -> "Opus RLD"
    else -> "Unknown"
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
    var selectedTab by remember { mutableIntStateOf(0) } // 0=Connect, 1=Studio DSP, 2=Cast & Party
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
    var castPhoneInternalAudio by remember { mutableStateOf(false) }
    var activeCaster by remember { mutableStateOf<PhoneAudioCaster?>(null) }
    var sleepTimerMinutes by remember { mutableIntStateOf(0) } // 0=Off, 15, 30, 60m
    // Index contract matches crates/android-jni/src/lib.rs:
    // 0..10 legacy counters, 11..21 live latency breakdown.
    var telemetry by remember { mutableStateOf(LongArray(24)) }
    var btEarbudsDetected by remember { mutableStateOf(false) }

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

    // The foreground service must hold the MICROPHONE type before a mic or
    // internal-audio capture starts, especially with targetSdk 34 and the
    // screen off. Plain playback never calls this.
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

    val projectionLauncher = rememberLauncherForActivityResult(
        contract = ActivityResultContracts.StartActivityForResult()
    ) { result ->
        if (result.resultCode == Activity.RESULT_OK && result.data != null && Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            enterCaptureForeground("Party Audio Broadcast Active")
            val mgr = context.getSystemService(Context.MEDIA_PROJECTION_SERVICE) as MediaProjectionManager
            val proj = mgr.getMediaProjection(result.resultCode, result.data!!)
            if (proj != null) {
                val targets = hostAddress.split(',', ';', ' ').map { it.trim() }.filter { it.isNotEmpty() }
                activeCaster = PhoneAudioCaster(proj, if (targets.isEmpty()) listOf(hostAddress) else targets).also { it.startCasting() }
                castPhoneInternalAudio = true
            } else {
                exitCaptureForeground(sharePhoneMic)
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
            val mgr = context.getSystemService(Context.MEDIA_PROJECTION_SERVICE) as MediaProjectionManager
            projectionLauncher.launch(mgr.createScreenCaptureIntent())
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

            // Only treat Bluetooth as active when media audio is actually
            // routed there. `getDevices(GET_DEVICES_OUTPUTS)` lists every
            // connected device, so a paired-but-idle headset used to report
            // +145ms even while the phone played through its own speaker.
            val hasBt = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
                // Query the same usage/content type the AAudio stream was
                // opened with. Raw mode uses GAME/SONIFICATION, so querying
                // MEDIA/MUSIC could report the speaker while audio actually
                // plays through Bluetooth.
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
                // API < 31 has no per-attributes routing query, and BLE audio
                // does not exist before API 31. The deprecated routing flags
                // are the only way to distinguish a routed A2DP/SCO headset
                // from one that is merely paired and idle.
                @Suppress("DEPRECATION")
                val a2dpRouted = audioManager.isBluetoothA2dpOn
                @Suppress("DEPRECATION")
                val scoRouted = audioManager.isBluetoothScoOn()
                a2dpRouted || scoRouted
            }
            btEarbudsDetected = hasBt
            // Report genuine downstream Bluetooth latency back to the PC
            // host for the video-sync helper. 0 means "no BT route"; never
            // fake 8ms of Bluetooth latency.
            AudioEngineService.nativeReportBluetoothDelayMs(if (hasBt) 145 else 0)
            delay(250)
        }
    }

    Scaffold(
        topBar = {
            TopAppBar(
                title = { Text("AudioShare Ultra • Lossless Engine") }
            )
        }
    ) { padding ->
        Column(
            modifier = Modifier
                .fillMaxSize()
                .padding(padding)
                .padding(16.dp)
                .verticalScroll(rememberScrollState()),
            verticalArrangement = Arrangement.spacedBy(16.dp)
        ) {
            // Persistent Top Live Status & Warning Banner
            ElevatedCard(modifier = Modifier.fillMaxWidth()) {
                Column(modifier = Modifier.padding(12.dp), verticalArrangement = Arrangement.spacedBy(4.dp)) {
                    val authErr = telemetry.getOrElse(5) { 0L }
                    val idleSecs = telemetry.getOrElse(6) { 0L }
                    val dtxSilent = telemetry.getOrElse(7) { 0L } == 1L
                    val hostVol = telemetry.getOrElse(8) { 100L }
                    val groupDelay = telemetry.getOrElse(9) { 0L }

                    val statusTitle = when {
                        !isConnected -> "⚪ Disconnected — Tap a PC below or scan QR"
                        authErr > 5 -> "🔴 Wrong Encryption PIN! ($authErr packets rejected)"
                        idleSecs >= 3 -> "⚠️ Host Signal Lost (${idleSecs}s) — Auto-Reconnecting..."
                        dtxSilent -> "🔋 Smart DTX Sleep (-90dBFS BT Keep-Alive Active)"
                        else -> "🟢 Streaming 48kHz Lossless • Host Vol: ${hostVol}%"
                    }
                    Text(statusTitle, style = MaterialTheme.typography.titleSmall)
                    if (isConnected) {
                        Text(
                            "Pkts: ${telemetry[0]} | FEC: ${telemetry[2]} | Drift: ${telemetry[4]} ppm | Sync: +${groupDelay}ms",
                            fontFamily = FontFamily.Monospace,
                            style = MaterialTheme.typography.bodySmall
                        )
                        if (latencyDebug) {
                            val estMs = telemetry.getOrElse(20) { 0L } / 10.0
                            val targetMs = telemetry.getOrElse(14) { 0L } / 1000.0
                            val bufferedMs = telemetry.getOrElse(15) { 0L } / 1000.0
                            Text(
                                "📊 Est: ${"%.1f".format(estMs)} ms | FIFO: ${"%.1f".format(bufferedMs)}/${"%.1f".format(targetMs)} ms | DAC: ${telemetry.getOrElse(10) { 0L }} ms | BT: ${telemetry.getOrElse(17) { 0L }} ms",
                                fontFamily = FontFamily.Monospace,
                                style = MaterialTheme.typography.bodySmall,
                                color = MaterialTheme.colorScheme.tertiary
                            )
                        }
                    }
                }
            }

            // 3-Tab Material 3 Navigation Bar
            TabRow(selectedTabIndex = selectedTab) {
                Tab(selected = selectedTab == 0, onClick = { selectedTab = 0 }, text = { Text("📡 Connect") })
                Tab(selected = selectedTab == 1, onClick = { selectedTab = 1 }, text = { Text("🎛️ Studio DSP") })
                Tab(selected = selectedTab == 2, onClick = { selectedTab = 2 }, text = { Text("🎙️ Cast & Party") })
            }

            if (selectedTab == 0) {
                // 0. 1-Tap OEM Battery / Doze Exemption Banner (Prevents screen-off 2.4GHz packet coalescing!)
                if (!isBatteryUnrestricted && Build.VERSION.SDK_INT >= Build.VERSION_CODES.M) {
                    ElevatedCard(modifier = Modifier.fillMaxWidth()) {
                        Row(
                            modifier = Modifier.fillMaxWidth().padding(12.dp),
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

                // 1. Host Connection Card
                ElevatedCard(modifier = Modifier.fillMaxWidth()) {
                    Column(
                        modifier = Modifier.padding(16.dp),
                        verticalArrangement = Arrangement.spacedBy(12.dp)
                    ) {
                        Text("PC Host Endpoint (USB RNDIS or 5GHz Wi-Fi)", style = MaterialTheme.typography.titleMedium)
                        OutlinedTextField(
                            value = hostAddress,
                            onValueChange = { hostAddress = it },
                            label = { Text("Host IP:Port") },
                            singleLine = true,
                            modifier = Modifier.fillMaxWidth()
                        )

                        if (discoveredHosts.isNotEmpty()) {
                            Text(
                                "Auto-Discovered PC Hosts (tap to select)",
                                style = MaterialTheme.typography.bodySmall
                            )
                            discoveredHosts.forEach { entry ->
                                val parts = entry.split("|")
                                val name = parts.getOrNull(0) ?: entry
                                val addr = parts.getOrNull(1) ?: entry
                                val mode = parts.getOrNull(2) ?: "WiFi"
                                val icon = if (name.contains("Party")) "📱 Party Phone" else "💻 PC"
                                OutlinedButton(
                                    onClick = {
                                        hostAddress = addr
                                        prefs.edit().putString("last_host_addr", hostAddress).putString("pin_code", pinCode).apply()
                                        val intent = Intent(context, AudioEngineService::class.java).apply {
                                            putExtra("HOST_ADDR", hostAddress)
                                            // The scanner already knows whether this
                                            // host was discovered over USB. Forward
                                            // it so Android 11+ randomized RNDIS
                                            // subnets still get the USB jitter buffer.
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
                                    Text("⚡ $icon [$mode]: $name ($addr)")
                                }
                            }
                        }

                        OutlinedTextField(
                            value = pinCode,
                            onValueChange = {
                                pinCode = it
                                prefs.edit().putString("pin_code", it).apply()
                            },
                            label = { Text("PIN (optional, ChaCha20 + MAC32)") },
                            singleLine = true,
                            modifier = Modifier.fillMaxWidth()
                        )

                        Row(horizontalArrangement = Arrangement.spacedBy(12.dp)) {
                            Button(
                                onClick = {
                                    prefs.edit().putString("last_host_addr", hostAddress).apply()
                                    prefs.edit().putString("pin_code", pinCode).apply()
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
                                modifier = Modifier.weight(1f)
                            ) {
                                Text(if (isConnected) "Disconnect" else "Connect Lossless Stream")
                            }

                            OutlinedButton(
                                onClick = { hostAddress = "127.0.0.1:65530" }
                            ) {
                                Text("USB ADB")
                            }

                            // In-App 1-Tap QR Camera Launcher
                            OutlinedButton(
                                onClick = {
                                    try {
                                        val qrIntent = Intent("com.google.zxing.client.android.SCAN")
                                        context.startActivity(qrIntent)
                                    } catch (_: Exception) {
                                        val camIntent = Intent(MediaStore.INTENT_ACTION_STILL_IMAGE_CAMERA)
                                        context.startActivity(camIntent)
                                    }
                                }
                            ) {
                                Text("📷 QR")
                            }
                        }
                    }
                }

                // 2. Live Real-Time Sequencer & Clock Drift HUD
                ElevatedCard(modifier = Modifier.fillMaxWidth()) {
                    Column(
                        modifier = Modifier.padding(16.dp),
                        verticalArrangement = Arrangement.spacedBy(6.dp)
                    ) {
                        Row(
                            modifier = Modifier.fillMaxWidth(),
                            horizontalArrangement = Arrangement.SpaceBetween,
                            verticalAlignment = Alignment.CenterVertically
                        ) {
                            Text("Real-Time Hardware Telemetry", style = MaterialTheme.typography.titleMedium)
                            FilterChip(
                                selected = latencyDebug,
                                onClick = {
                                    val next = !latencyDebug
                                    latencyDebug = next
                                    prefs.edit().putBoolean("latency_debug", next).apply()
                                },
                                label = { Text("📊 Debug") }
                            )
                        }
                        Text("RX Packets        : ${telemetry[0]}", fontFamily = FontFamily.Monospace)
                        Text("Lossless Subframes: ${telemetry[1]}", fontFamily = FontFamily.Monospace)
                        Text("FEC Healed Drops  : ${telemetry[2]}", fontFamily = FontFamily.Monospace)
                        Text("WSOLA PLC Frames  : ${telemetry[3]}", fontFamily = FontFamily.Monospace)
                        Text("Clock Drift Lock  : ${telemetry[4]} ppm", fontFamily = FontFamily.Monospace)
                        Text("AAudio HW Latency : ${telemetry.getOrElse(10) { 0L }} ms", fontFamily = FontFamily.Monospace)
                        Text("Host Silence      : ${telemetry.getOrElse(6) { 0L }} s", fontFamily = FontFamily.Monospace)
                        if (telemetry.getOrElse(5) { 0L } > 0) {
                            Text("Auth Failures     : ${telemetry[5]}", color = MaterialTheme.colorScheme.error, fontFamily = FontFamily.Monospace)
                        }
                        if (btEarbudsDetected) {
                            Text(
                                "🎧 Bluetooth Earbuds Active: Reporting +145ms A/V Sync Offset to PC",
                                color = MaterialTheme.colorScheme.tertiary,
                                style = MaterialTheme.typography.bodySmall
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

                            Spacer(Modifier.height(4.dp))
                            Text("📊 Live Latency Breakdown", style = MaterialTheme.typography.titleSmall)
                            Text(
                                "Estimated one-way  : ${"%.1f".format(estMs)} ms",
                                fontFamily = FontFamily.Monospace,
                                color = MaterialTheme.colorScheme.tertiary
                            )
                            Text("Sender packetization: ${usToMs(packetUs)} ms", fontFamily = FontFamily.Monospace)
                            Text("Network jitter     : ${usToMs(jitterUs)} ms", fontFamily = FontFamily.Monospace)
                            Text("Adaptive cushion   : ${usToMs(adaptiveUs)} ms", fontFamily = FontFamily.Monospace)
                            Text("Target FIFO        : ${usToMs(targetUs)} ms", fontFamily = FontFamily.Monospace)
                            Text(
                                "Buffered right now : ${usToMs(bufferedUs)} ms (${"%.0f".format(fillPct)}%)",
                                fontFamily = FontFamily.Monospace
                            )
                            Text("Group sync addition: +${usToMs(groupUs)} ms", fontFamily = FontFamily.Monospace)
                            Text("DAC queue          : ${telemetry.getOrElse(10) { 0L }} ms", fontFamily = FontFamily.Monospace)
                            Text("Codec lookahead    : ${"%.1f".format(codecLookaheadMs)} ms", fontFamily = FontFamily.Monospace)
                            Text("Bluetooth estimate : $btLabel", fontFamily = FontFamily.Monospace)
                            Text("Prebuffer          : $prebufferLabel", fontFamily = FontFamily.Monospace)
                            Text("Codec              : ${codecModeName(codecMode)}", fontFamily = FontFamily.Monospace)
                            Text(
                                "Estimate = packetization + target FIFO + max(DAC, BT, output) + codec lookahead. Group sync is already inside the FIFO. Excludes one-way network transit and the acoustic air path; use the sonar for end-to-end.",
                                style = MaterialTheme.typography.bodySmall
                            )
                        }
                    }
                }
            } // End Tab 0

            if (selectedTab == 1) {
                // Studio Pre-Amp & True-Peak Limiter Card (#64 & #34)
                ElevatedCard(modifier = Modifier.fillMaxWidth()) {
                    Column(
                        modifier = Modifier.padding(16.dp),
                        verticalArrangement = Arrangement.spacedBy(8.dp)
                    ) {
                        // 1-Tap Studio EQ Presets
                        Text("✨ 1-Tap Studio EQ Presets:", style = MaterialTheme.typography.labelMedium)
                        Row(modifier = Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.SpaceBetween) {
                            listOf(
                                Triple("Flat", 0f, 0f) to 0f,
                                Triple("🔥 Bass", 6.5f, 0f) to 1.5f,
                                Triple("🎙️ Vocal", -2f, 5.5f) to 2f,
                                Triple("🎮 FPS", -3f, 6f) to 4.5f
                            ).forEach { (b_v, t) ->
                                OutlinedButton(
                                    onClick = {
                                        bassDb = b_v.second
                                        vocalDb = b_v.third
                                        trebleDb = t
                                        syncAndSaveDsp()
                                    }
                                ) { Text(b_v.first) }
                            }
                        }

                        Row(
                            modifier = Modifier.fillMaxWidth(),
                            horizontalArrangement = Arrangement.SpaceBetween,
                            verticalAlignment = Alignment.CenterVertically
                        ) {
                            Text("Studio Pre-Amp (True-Peak Limited)", style = MaterialTheme.typography.titleMedium)
                            Text("${"%.1f".format(preampGainDb)} dB", fontFamily = FontFamily.Monospace)
                        }
                        Slider(
                            value = preampGainDb,
                            onValueChange = {
                                preampGainDb = it
                                syncAndSaveDsp()
                            },
                            valueRange = -12.0f..24.0f
                        )

                        // Host quality profile selector (sent as feedback mode 1..5)
                        Text("Host Quality Mode", style = MaterialTheme.typography.titleSmall)
                        Row(
                            modifier = Modifier
                                .fillMaxWidth()
                                .horizontalScroll(rememberScrollState()),
                            horizontalArrangement = Arrangement.spacedBy(6.dp)
                        ) {
                            listOf(
                                0 to "🤖 Auto",
                                1 to "💎 16-Bit",
                                2 to "⚡ VBR",
                                3 to "🛡️ 10ms 2.4G",
                                4 to "👑 24-Bit"
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

                        // Bitrate selector for the compressed profiles. The
                        // host resolves the command to 96/160/256/320 kbps and
                        // ignores it while a lossless profile is active.
                        if (qualityMode == 0 || qualityMode == 2) {
                            Text("Compressed Bitrate", style = MaterialTheme.typography.titleSmall)
                            Row(
                                modifier = Modifier
                                    .fillMaxWidth()
                                    .horizontalScroll(rememberScrollState()),
                                horizontalArrangement = Arrangement.spacedBy(6.dp)
                            ) {
                                listOf(96, 160, 256, 320).forEachIndexed { idx, kbps ->
                                    FilterChip(
                                        selected = compressedBitrateKbps == kbps,
                                        onClick = {
                                            compressedBitrateKbps = kbps
                                            prefs.edit().putInt("target_bitrate_kbps", kbps).apply()
                                            AudioEngineService.nativeSendMediaCommand(30 + idx)
                                        },
                                        label = { Text("$kbps kbps") }
                                    )
                                }
                            }
                        }

                        // Separate compressed-codec backend switch. Lossless
                        // profiles ignore it; the host can switch live without
                        // restarting the client.
                        Text("Compressed Codec Module", style = MaterialTheme.typography.titleSmall)
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
                                label = { Text("🦀 Rust VBR (Default)") }
                            )
                            FilterChip(
                                selected = useOpus,
                                onClick = {
                                    useOpus = true
                                    prefs.edit().putBoolean("use_opus", true).apply()
                                    AudioEngineService.nativeSendMediaCommand(21)
                                },
                                label = { Text("🎵 Opus RLD (Pure Rust)") }
                            )
                        }

                        Row(
                            modifier = Modifier
                                .fillMaxWidth()
                                .horizontalScroll(rememberScrollState()),
                            horizontalArrangement = Arrangement.spacedBy(6.dp)
                        ) {
                            FilterChip(
                                selected = nightMode,
                                onClick = {
                                    nightMode = !nightMode
                                    syncAndSaveDsp()
                                },
                                label = { Text("🌙 Night") }
                            )
                            FilterChip(
                                selected = crossfeed,
                                onClick = {
                                    crossfeed = !crossfeed
                                    syncAndSaveDsp()
                                },
                                label = { Text("🎧 3D") }
                            )
                            FilterChip(
                                selected = gamingLock,
                                onClick = {
                                    gamingLock = !gamingLock
                                    syncAndSaveDsp()
                                },
                                label = { Text("🎮 5ms Lock") }
                            )
                            FilterChip(
                                selected = channelRole != 0,
                                onClick = {
                                    channelRole = (channelRole + 1) % 5
                                    syncAndSaveDsp()
                                },
                                label = {
                                    val roleName = listOf("Stereo", "Left", "Right", "Vocal", "Surround")[channelRole]
                                    Text("🔊 $roleName")
                                }
                            )
                            FilterChip(
                                selected = dolbyEffects,
                                onClick = { setDolbyEffects(!dolbyEffects) },
                                label = { Text("🎬 Dolby") }
                            )
                        }

                        Row(
                            modifier = Modifier.fillMaxWidth(),
                            horizontalArrangement = Arrangement.SpaceEvenly
                        ) {
                            OutlinedButton(onClick = { AudioEngineService.nativeSendMediaCommand(12) }) { Text("⏮ Prev") }
                            OutlinedButton(onClick = { AudioEngineService.nativeSendMediaCommand(10) }) { Text("⏯ Play/Pause PC") }
                            OutlinedButton(onClick = { AudioEngineService.nativeSendMediaCommand(11) }) { Text("⏭ Next") }
                        }

                        // Sleep Timer Selector (Off / 15m / 30m / 60m)
                        Row(
                            modifier = Modifier.fillMaxWidth(),
                            horizontalArrangement = Arrangement.SpaceBetween,
                            verticalAlignment = Alignment.CenterVertically
                        ) {
                            Text("⏲️ Sleep Timer:", style = MaterialTheme.typography.labelMedium)
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

                // Studio 3-Band Parametric Equalizer Card
                ElevatedCard(modifier = Modifier.fillMaxWidth()) {
                    Column(
                        modifier = Modifier.padding(16.dp),
                        verticalArrangement = Arrangement.spacedBy(8.dp)
                    ) {
                        Text("Studio 3-Band Parametric EQ", style = MaterialTheme.typography.titleMedium)

                        Row(modifier = Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.SpaceBetween) {
                            Text("Sub-Bass (80 Hz)", style = MaterialTheme.typography.bodySmall)
                            Text("${"%.1f".format(bassDb)} dB", fontFamily = FontFamily.Monospace, style = MaterialTheme.typography.bodySmall)
                        }
                        Slider(
                            value = bassDb,
                            onValueChange = {
                                bassDb = it
                                syncAndSaveDsp()
                            },
                            valueRange = -12.0f..12.0f
                        )

                        Row(modifier = Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.SpaceBetween) {
                            Text("Vocal Presence (2.5 kHz)", style = MaterialTheme.typography.bodySmall)
                            Text("${"%.1f".format(vocalDb)} dB", fontFamily = FontFamily.Monospace, style = MaterialTheme.typography.bodySmall)
                        }
                        Slider(
                            value = vocalDb,
                            onValueChange = {
                                vocalDb = it
                                syncAndSaveDsp()
                            },
                            valueRange = -12.0f..12.0f
                        )

                        Row(modifier = Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.SpaceBetween) {
                            Text("Treble Air (10 kHz)", style = MaterialTheme.typography.bodySmall)
                            Text("${"%.1f".format(trebleDb)} dB", fontFamily = FontFamily.Monospace, style = MaterialTheme.typography.bodySmall)
                        }
                        Slider(
                            value = trebleDb,
                            onValueChange = {
                                trebleDb = it
                                syncAndSaveDsp()
                            },
                            valueRange = -12.0f..12.0f
                        )
                    }
                }
            } // End Tab 1

            if (selectedTab == 2) {
                ElevatedCard(modifier = Modifier.fillMaxWidth()) {
                    Column(modifier = Modifier.padding(14.dp), verticalArrangement = Arrangement.spacedBy(4.dp)) {
                        Text("🎙️ Cast & Party Broadcast", style = MaterialTheme.typography.titleMedium)
                        Text(
                            "Stream your microphone to Zoom/Discord/Meet on PC, or broadcast internal music & gaming audio across multiple phones. Other phones discover this device automatically as 'Android-Party-Host'.",
                            style = MaterialTheme.typography.bodySmall
                        )
                    }
                }

                // Cast Phone Internal Media & Games (PhoneAudioCaster - Party Mode)
                if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
                    ElevatedCard(modifier = Modifier.fillMaxWidth()) {
                        Row(
                            modifier = Modifier
                                .fillMaxWidth()
                                .padding(16.dp),
                            horizontalArrangement = Arrangement.SpaceBetween,
                            verticalAlignment = Alignment.CenterVertically
                        ) {
                            Column(modifier = Modifier.weight(1f)) {
                                Text("Cast Phone Internal Audio (Party Mode)", style = MaterialTheme.typography.titleMedium)
                                Text(
                                    "Broadcasts media & games directly from this phone to PC headphones or multiple phones",
                                    style = MaterialTheme.typography.bodySmall
                                )
                            }
                            Switch(
                                checked = castPhoneInternalAudio,
                                onCheckedChange = { enabled ->
                                    if (enabled) {
                                        val hasPerm = ContextCompat.checkSelfPermission(
                                            context,
                                            Manifest.permission.RECORD_AUDIO
                                        ) == PackageManager.PERMISSION_GRANTED
                                        if (hasPerm) {
                                            val mgr = context.getSystemService(Context.MEDIA_PROJECTION_SERVICE) as MediaProjectionManager
                                            projectionLauncher.launch(mgr.createScreenCaptureIntent())
                                        } else {
                                            pendingInternalAudioCast = true
                                            castAudioPermissionLauncher.launch(Manifest.permission.RECORD_AUDIO)
                                        }
                                    } else {
                                        activeCaster?.stopCasting()
                                        activeCaster = null
                                        castPhoneInternalAudio = false
                                        AudioEngineService.nativeStopPhoneMic()
                                        exitCaptureForeground(sharePhoneMic)
                                    }
                                }
                            )
                        }
                    }
                }

                // Bidirectional Phone-as-PC-Microphone Card (#27)
                ElevatedCard(modifier = Modifier.fillMaxWidth()) {
                    Row(
                        modifier = Modifier
                            .fillMaxWidth()
                            .padding(16.dp),
                        horizontalArrangement = Arrangement.SpaceBetween,
                        verticalAlignment = Alignment.CenterVertically
                    ) {
                        Column(modifier = Modifier.weight(1f)) {
                            Text("Share Phone Microphone / Audio (Party Mode)", style = MaterialTheme.typography.titleMedium)
                            Text(
                                "Streams 48kHz low-latency AAudio Mic or Internal Cast to PC or multiple phones (Port 65532)",
                                style = MaterialTheme.typography.bodySmall
                            )
                        }
                        Switch(
                            checked = sharePhoneMic,
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
                }
            } // End Tab 2
        }
    }
}
