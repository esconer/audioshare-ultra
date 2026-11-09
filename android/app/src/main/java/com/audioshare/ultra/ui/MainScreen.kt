package com.audioshare.ultra.ui

import android.Manifest
import android.app.Activity
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
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
    var channelRole by remember { mutableIntStateOf(prefs.getInt("channel_role", 0)) }
    // 0=Smart Auto, 1=Studio Lossless 16-bit, 2=Low-Latency VBR,
    // 3=Max Stability (10ms 2.4GHz), 4=Studio Master 24-bit
    var qualityMode by remember { mutableIntStateOf(prefs.getInt("quality_mode", 0)) }
    var sharePhoneMic by remember { mutableStateOf(false) }
    var castPhoneInternalAudio by remember { mutableStateOf(false) }
    var activeCaster by remember { mutableStateOf<PhoneAudioCaster?>(null) }
    var sleepTimerMinutes by remember { mutableIntStateOf(0) } // 0=Off, 15, 30, 60m
    var telemetry by remember { mutableStateOf(LongArray(11)) }
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
            .putInt("channel_role", channelRole)
            .apply()
        AudioEngineService.nativeSetGainDb(preampGainDb)
        AudioEngineService.nativeConfigureDsp(bassDb, vocalDb, trebleDb, channelRole, crossfeed, nightMode, gamingLock)
    }

    val micPermissionLauncher = rememberLauncherForActivityResult(
        contract = ActivityResultContracts.RequestPermission()
    ) { granted ->
        if (granted) {
            sharePhoneMic = true
            if (!isConnected) {
                val guardIntent = Intent(context, AudioEngineService::class.java).apply {
                    action = "ACTION_START_CAST_GUARD"
                    putExtra("CAST_LABEL", "PC Video-Call Mic Active")
                }
                if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                    context.startForegroundService(guardIntent)
                } else {
                    context.startService(guardIntent)
                }
            }
            AudioEngineService.nativeStartPhoneMic(hostAddress, true)
        } else {
            sharePhoneMic = false
        }
    }

    val projectionLauncher = rememberLauncherForActivityResult(
        contract = ActivityResultContracts.StartActivityForResult()
    ) { result ->
        if (result.resultCode == Activity.RESULT_OK && result.data != null && Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            if (!isConnected) {
                val guardIntent = Intent(context, AudioEngineService::class.java).apply {
                    action = "ACTION_START_CAST_GUARD"
                    putExtra("CAST_LABEL", "Party Audio Broadcast Active")
                }
                if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                    context.startForegroundService(guardIntent)
                } else {
                    context.startService(guardIntent)
                }
            }
            val mgr = context.getSystemService(Context.MEDIA_PROJECTION_SERVICE) as MediaProjectionManager
            val proj = mgr.getMediaProjection(result.resultCode, result.data!!)
            if (proj != null) {
                val targets = hostAddress.split(',', ';', ' ').map { it.trim() }.filter { it.isNotEmpty() }
                activeCaster = PhoneAudioCaster(proj, if (targets.isEmpty()) listOf(hostAddress) else targets).also { it.startCasting() }
                castPhoneInternalAudio = true
            }
        } else {
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
    LaunchedEffect(isConnected) {
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

            val devices = audioManager.getDevices(AudioManager.GET_DEVICES_OUTPUTS)
            val hasBt = devices.any {
                it.type == AudioDeviceInfo.TYPE_BLUETOOTH_A2DP ||
                    it.type == AudioDeviceInfo.TYPE_BLE_HEADSET
            }
            btEarbudsDetected = hasBt
            // Report downstream Bluetooth A2DP latency back to PC Host for Video Sync helper
            AudioEngineService.nativeReportBluetoothDelayMs(if (hasBt) 145 else 8)
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
                        Text("Real-Time Hardware Telemetry", style = MaterialTheme.typography.titleMedium)
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

                        Row(
                            modifier = Modifier.fillMaxWidth(),
                            horizontalArrangement = Arrangement.SpaceBetween
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
                                        val mgr = context.getSystemService(Context.MEDIA_PROJECTION_SERVICE) as MediaProjectionManager
                                        projectionLauncher.launch(mgr.createScreenCaptureIntent())
                                    } else {
                                        activeCaster?.stopCasting()
                                        activeCaster = null
                                        castPhoneInternalAudio = false
                                        AudioEngineService.nativeStopPhoneMic()
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
                                } else {
                                    val hasPerm = ContextCompat.checkSelfPermission(
                                        context,
                                        Manifest.permission.RECORD_AUDIO
                                    ) == PackageManager.PERMISSION_GRANTED
                                    if (hasPerm) {
                                        sharePhoneMic = true
                                        if (!isConnected) {
                                            val guardIntent = Intent(context, AudioEngineService::class.java).apply {
                                                action = "ACTION_START_CAST_GUARD"
                                                putExtra("CAST_LABEL", "PC Video-Call Mic Active")
                                            }
                                            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                                                context.startForegroundService(guardIntent)
                                            } else {
                                                context.startService(guardIntent)
                                            }
                                        }
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
