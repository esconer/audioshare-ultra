package com.audioshare.ultra.ui

import android.app.Activity
import android.content.Context
import android.content.Intent
import android.media.AudioDeviceInfo
import android.media.AudioManager
import android.media.projection.MediaProjectionManager
import android.os.Build
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
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

    var hostAddress by remember {
        mutableStateOf(prefs.getString("last_host_addr", "192.168.42.129:65530") ?: "192.168.42.129:65530")
    }
    var pinCode by remember {
        mutableStateOf(prefs.getString("pin_code", "") ?: "")
    }
    var isConnected by remember { mutableStateOf(AudioEngineService.isStreaming) }
    var preampGainDb by remember { mutableFloatStateOf(0.0f) }
    var bassDb by remember { mutableFloatStateOf(0.0f) }
    var vocalDb by remember { mutableFloatStateOf(0.0f) }
    var trebleDb by remember { mutableFloatStateOf(0.0f) }
    var nightMode by remember { mutableStateOf(false) }
    var crossfeed by remember { mutableStateOf(false) }
    var channelRole by remember { mutableIntStateOf(0) }
    var sharePhoneMic by remember { mutableStateOf(false) }
    var castPhoneInternalAudio by remember { mutableStateOf(false) }
    var activeCaster by remember { mutableStateOf<PhoneAudioCaster?>(null) }
    var telemetry by remember { mutableStateOf(LongArray(5)) }
    var btEarbudsDetected by remember { mutableStateOf(false) }

    val projectionLauncher = rememberLauncherForActivityResult(
        contract = ActivityResultContracts.StartActivityForResult()
    ) { result ->
        if (result.resultCode == Activity.RESULT_OK && result.data != null && Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
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
                            OutlinedButton(
                                onClick = { hostAddress = addr },
                                modifier = Modifier.fillMaxWidth()
                            ) {
                                Text("$name • $mode • $addr")
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
                    }
                }
            }

            // 2. Studio Pre-Amp & True-Peak Limiter Card (#64 & #34)
            ElevatedCard(modifier = Modifier.fillMaxWidth()) {
                Column(
                    modifier = Modifier.padding(16.dp),
                    verticalArrangement = Arrangement.spacedBy(8.dp)
                ) {
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
                            if (isConnected) AudioEngineService.nativeSetGainDb(it)
                        },
                        valueRange = -12.0f..24.0f
                    )

                    Row(
                        modifier = Modifier.fillMaxWidth(),
                        horizontalArrangement = Arrangement.SpaceBetween
                    ) {
                        FilterChip(
                            selected = nightMode,
                            onClick = {
                                nightMode = !nightMode
                                AudioEngineService.nativeConfigureDsp(bassDb, vocalDb, trebleDb, channelRole, crossfeed, nightMode)
                            },
                            label = { Text("🌙 Night Mode") }
                        )
                        FilterChip(
                            selected = crossfeed,
                            onClick = {
                                crossfeed = !crossfeed
                                AudioEngineService.nativeConfigureDsp(bassDb, vocalDb, trebleDb, channelRole, crossfeed, nightMode)
                            },
                            label = { Text("🎧 Binaural 3D") }
                        )
                        FilterChip(
                            selected = channelRole != 0,
                            onClick = {
                                channelRole = (channelRole + 1) % 5
                                AudioEngineService.nativeConfigureDsp(bassDb, vocalDb, trebleDb, channelRole, crossfeed, nightMode)
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
                }
            }

            // 3. Studio 3-Band Parametric Equalizer Card
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
                            AudioEngineService.nativeConfigureDsp(bassDb, vocalDb, trebleDb, channelRole, crossfeed, nightMode)
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
                            AudioEngineService.nativeConfigureDsp(bassDb, vocalDb, trebleDb, channelRole, crossfeed, nightMode)
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
                            AudioEngineService.nativeConfigureDsp(bassDb, vocalDb, trebleDb, channelRole, crossfeed, nightMode)
                        },
                        valueRange = -12.0f..12.0f
                    )
                }
            }

            // 4. Cast Phone Internal Media & Games (PhoneAudioCaster - Party Mode)
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

            // 3. Bidirectional Phone-as-PC-Microphone Card (#27)
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
                            sharePhoneMic = enabled
                            if (enabled && isConnected) {
                                AudioEngineService.nativeStartPhoneMic(hostAddress)
                            } else if (!enabled) {
                                AudioEngineService.nativeStopPhoneMic()
                            }
                        }
                    )
                }
            }

            // 4. Live Real-Time Sequencer & Clock Drift HUD
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
                    if (btEarbudsDetected) {
                        Text(
                            "🎧 Bluetooth Earbuds Active: Reporting +145ms A/V Sync Offset to PC",
                            color = MaterialTheme.colorScheme.tertiary,
                            style = MaterialTheme.typography.bodySmall
                        )
                    }
                }
            }
        }
    }
}
