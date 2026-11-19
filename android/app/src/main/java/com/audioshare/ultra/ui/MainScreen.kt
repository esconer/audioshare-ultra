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
import android.view.KeyEvent
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
import androidx.compose.ui.platform.LocalClipboardManager
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.core.content.ContextCompat
import com.audioshare.ultra.service.AudioEngineService
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
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

@OptIn(ExperimentalMaterial3Api::class, ExperimentalLayoutApi::class)
@Composable
fun MainScreen() {
    val context = LocalContext.current
    val clipboard = LocalClipboardManager.current
    val snackbarHostState = remember { SnackbarHostState() }
    val scope = rememberCoroutineScope()
    var showQrScanner by remember { mutableStateOf(false) }
    var showToolsSheet by remember { mutableStateOf(false) }
    var pendingEncryptedHost by remember { mutableStateOf<String?>(null) }
    var promptPinInput by remember { mutableStateOf("") }
    var vuPeakHold by remember { mutableFloatStateOf(0f) }

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
    var useOpus by remember { mutableStateOf(prefs.getBoolean("use_opus", true)) }
    var btDelayOverrideMs by remember {
        mutableIntStateOf(prefs.getInt("bt_delay_override_ms", 0))
    }
    // The service owns the phone-mic lifecycle (ACTION_START/STOP_PHONE_MIC).
    // Mirror its broadcast-backed state so the switch survives UI recreation.
    var sharePhoneMic by remember { mutableStateOf(AudioEngineService.isPhoneMicActive) }
    // The service owns the MediaProjection token and the caster. The UI only
    // mirrors the service's broadcast so a system-revoked projection turns
    // the switch off even if the app was backgrounded.
    var castPhoneInternalAudio by remember { mutableStateOf(AudioEngineService.isPartyCasting) }
    var forwardPartyToPc by remember { mutableStateOf(prefs.getBoolean("party_forward_pc", false)) }
    var muteHostSpeakerInParty by remember { mutableStateOf(prefs.getBoolean("party_mute_local", false)) }
    var savedMusicVolume by remember { mutableIntStateOf(-1) }
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

    fun applyLocalPartySpeakerMute(mute: Boolean) {
        val am = context.getSystemService(Context.AUDIO_SERVICE) as AudioManager
        if (mute) {
            val cur = am.getStreamVolume(AudioManager.STREAM_MUSIC)
            if (cur > 0) savedMusicVolume = cur
            am.setStreamVolume(AudioManager.STREAM_MUSIC, 0, 0)
        } else if (savedMusicVolume > 0) {
            am.setStreamVolume(AudioManager.STREAM_MUSIC, savedMusicVolume, 0)
            savedMusicVolume = -1
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
                    castPhoneInternalAudio = active
                    if (!active && muteHostSpeakerInParty) {
                        applyLocalPartySpeakerMute(false)
                    }
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
    fun connectToHost(addr: String, isUsb: Boolean, pin: String) {
        hostAddress = addr
        prefs.edit()
            .putString("last_host_addr", addr)
            .putString("pin_code", pin)
            .apply()
        val intent = Intent(context, AudioEngineService::class.java).apply {
            putExtra("HOST_ADDR", addr)
            putExtra("IS_USB", isUsb)
            putExtra("GAIN_DB", preampGainDb)
            putExtra("PIN", pin)
        }
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            context.startForegroundService(intent)
        } else {
            context.startService(intent)
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
                castPhoneInternalAudio = true
                if (muteHostSpeakerInParty) {
                    applyLocalPartySpeakerMute(true)
                }
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
        while (true) {
            isConnected = AudioEngineService.isStreaming
            discoveredHosts = runCatching {
                AudioEngineService.nativeGetDiscoveredHosts()
            }.getOrDefault("").lines().filter { it.isNotBlank() }
            delay(2000)
        }
    }

    // Poll 4Hz native telemetry & check if Bluetooth A2DP headphones are connected
    LaunchedEffect(isConnected, castPhoneInternalAudio, sharePhoneMic, dolbyEffects) {
        val audioManager = context.getSystemService(Context.AUDIO_SERVICE) as AudioManager
        while (isConnected || castPhoneInternalAudio || sharePhoneMic) {
            if (isConnected && !AudioEngineService.isStreaming) {
                isConnected = false
            }
            telemetry = AudioEngineService.nativeGetTelemetry()
            val remoteMediaCmd = ((telemetry.getOrElse(22) { 0L } ushr 32) and 0xFFL).toInt()
            if (remoteMediaCmd in 10..12) {
                val keyCode = when (remoteMediaCmd) {
                    10 -> KeyEvent.KEYCODE_MEDIA_PLAY_PAUSE
                    11 -> KeyEvent.KEYCODE_MEDIA_NEXT
                    else -> KeyEvent.KEYCODE_MEDIA_PREVIOUS
                }
                audioManager.dispatchMediaKeyEvent(KeyEvent(KeyEvent.ACTION_DOWN, keyCode))
                audioManager.dispatchMediaKeyEvent(KeyEvent(KeyEvent.ACTION_UP, keyCode))
            }

            // Auto-disconnect if PC host has been silent/unreachable for 5 minutes (300 seconds)
            val silenceSec = telemetry.getOrElse(6) { 0L }
            if (isConnected && silenceSec >= 300L && !sharePhoneMic && !castPhoneInternalAudio) {
                val stopIntent = Intent(context, AudioEngineService::class.java).apply {
                    action = "ACTION_STOP"
                }
                context.startService(stopIntent)
                isConnected = false
                break
            }

            if (!isConnected) {
                delay(250)
                continue
            }

            btEarbudsDetected = telemetry.getOrElse(17) { 0L } > 0L
            // Telemetry index 24 is the client's post-volume VU peak (0..100).
            val vuNow = telemetry.getOrElse(24) { 0L }.toInt().coerceIn(0, 100)
            vuPeakHold = if (vuNow >= vuPeakHold) {
                vuNow.toFloat()
            } else {
                (vuPeakHold - 3.0f).coerceAtLeast(0f)
            }
            // ~16Hz refresh while connected keeps the hero visualizer fluid.
            delay(if (isConnected) 60 else 250)
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
            val heroGlow = when {
                castPhoneInternalAudio && !isConnected -> Color(0xFFA855F7)
                sharePhoneMic && !isConnected -> StudioColors.Accent
                !isConnected -> StudioColors.Muted
                else -> StudioColors.LiveGlow
            }
            HeroAudioStage(
                glowColor = heroGlow,
                energyFraction = if (isConnected) {
                    telemetry.getOrElse(24) { 0L }.toInt().coerceIn(0, 100) / 100f
                } else 0f
            ) {
                val authErr = telemetry.getOrElse(5) { 0L }
                val idleSecs = telemetry.getOrElse(6) { 0L }
                val dtxSilent = telemetry.getOrElse(7) { 0L } == 1L
                val hostVol = telemetry.getOrElse(8) { 100L }
                val groupDelay = telemetry.getOrElse(9) { 0L }
                val codecMode = telemetry.getOrElse(19) { 0L }
                val partyPacked = telemetry.getOrElse(22) { 0L }
                val partyListeners = (partyPacked and 0xFFL).toInt()
                val partyCodec = (partyPacked shr 8) and 0xFFL
                val partySyncMs = telemetry.getOrElse(23) { 0L }

                val (statusTitle, dotColor) = when {
                    castPhoneInternalAudio && !isConnected ->
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
                        active = isConnected || castPhoneInternalAudio || sharePhoneMic,
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
                    if (sleepTimerMinutes > 0) {
                        StatusBadge(
                            text = "${sleepTimerMinutes}m",
                            containerColor = StudioColors.AccentDeep,
                            contentColor = StudioColors.AccentSoft
                        )
                    }
                }
                if (isConnected) {
                    val vuNow = telemetry.getOrElse(24) { 0L }.toInt().coerceIn(0, 100)
                    StudioLedVuMeter(vuPct = vuNow, peakHoldPct = vuPeakHold)
                    FluidAudioVisualizer(vuPct = vuNow, isLive = !dtxSilent)
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
                            prefs.edit()
                                .putString("last_host_addr", hostAddress)
                                .putString("pin_code", pinCode)
                                .apply()
                            val intent = Intent(context, AudioEngineService::class.java)
                            if (isConnected) {
                                intent.action = AudioEngineService.ACTION_STOP_CLIENT_ONLY
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
                            onClick = { hostAddress = "127.0.0.1:65530" },
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

                    Text("Sleep Timer:", style = MaterialTheme.typography.labelMedium)
                    FlowRow(horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                        listOf(0 to "Off", 15 to "15m", 30 to "30m", 60 to "60m").forEach { (mins, lbl) ->
                            FilterChip(
                                selected = (mins == 0 && sleepTimerMinutes == 0) ||
                                    (mins > 0 && sleepTimerMinutes > 0 && sleepTimerMinutes <= mins),
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

                    OutlinedButton(
                        onClick = {
                            val diag = runCatching { AudioEngineService.nativeGetDiagnostics() }
                                .getOrDefault("Diagnostics unavailable")
                            clipboard.setText(AnnotatedString(diag))
                            scope.launch {
                                snackbarHostState.showSnackbar("Diagnostics copied to clipboard")
                            }
                        },
                        modifier = Modifier.fillMaxWidth()
                    ) {
                        Icon(
                            Icons.Rounded.ContentCopy,
                            contentDescription = null,
                            modifier = Modifier.size(16.dp)
                        )
                        Spacer(Modifier.width(8.dp))
                        Text("Copy Diagnostics Report")
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
                            val activeBtMs = telemetry.getOrElse(17) { 185L }
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
                                TextButton(onClick = {
                                    btDelayOverrideMs = 0
                                    prefs.edit().putInt("bt_delay_override_ms", 0).apply()
                                    context.startService(
                                        Intent(context, AudioEngineService::class.java).apply {
                                            action = AudioEngineService.ACTION_SET_BT_OFFSET
                                        }
                                    )
                                }) { Text("Auto") }
                            }
                            Slider(
                                value = (if (btDelayOverrideMs == 0) activeBtMs else btDelayOverrideMs).toFloat(),
                                onValueChange = {
                                    val v = it.toInt().coerceIn(40, 350)
                                    btDelayOverrideMs = v
                                    prefs.edit().putInt("bt_delay_override_ms", v).apply()
                                    context.startService(
                                        Intent(context, AudioEngineService::class.java).apply {
                                            action = AudioEngineService.ACTION_SET_BT_OFFSET
                                        }
                                    )
                                },
                                valueRange = 40f..350f
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
                            qualityMode = 1
                            prefs.edit().putInt("quality_mode", 1).apply()
                            AudioEngineService.nativeSendMediaCommand(2)
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
                        badge = "Opus ${compressedBitrateKbps}k",
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
                                    if (muteHostSpeakerInParty) {
                                        applyLocalPartySpeakerMute(false)
                                    }
                                    val stopIntent = Intent(context, AudioEngineService::class.java).apply {
                                        action = AudioEngineService.ACTION_STOP_PARTY_CAST
                                    }
                                    context.startService(stopIntent)
                                }
                            }
                        )

                        SettingSwitchRow(
                            title = "Mute This Phone's Speaker During Party Cast",
                            subtitle = "Silences local speaker so all room audio comes from synced peer phones/PCs without echo",
                            checked = muteHostSpeakerInParty,
                            onCheckedChange = { mute ->
                                muteHostSpeakerInParty = mute
                                prefs.edit().putBoolean("party_mute_local", mute).apply()
                                if (castPhoneInternalAudio) {
                                    applyLocalPartySpeakerMute(mute)
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
                            val partyPacked = telemetry.getOrElse(22) { 0L }
                            val partyListeners = (partyPacked and 0xFFL).toInt()
                            val partyCodec = (partyPacked shr 8) and 0xFFL
                            val partyPort = ((partyPacked shr 16) and 0xFFFFL).let { if (it > 0) it else 65530L }
                            val partySyncMs = telemetry.getOrElse(23) { 0L }

                            Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                                HorizontalDivider(color = Color(0xFF243047))
                                Text(
                                    "Live Party Host Telemetry (Port $partyPort)",
                                    style = MaterialTheme.typography.labelMedium,
                                    color = MaterialTheme.colorScheme.secondary
                                )
                                Row(
                                    modifier = Modifier.fillMaxWidth(),
                                    horizontalArrangement = Arrangement.spacedBy(8.dp)
                                ) {
                                    MetricTile("Connected Peers", "$partyListeners", Modifier.weight(1f))
                                    MetricTile("Party Codec", codecModeName(partyCodec), Modifier.weight(1f))
                                    MetricTile("Group Sync", "+${partySyncMs} ms", Modifier.weight(1f))
                                }
                                Text(
                                    "Other phones & PCs can join by tapping 'Party Phone: Android-Party-Host' on their Connect tab.",
                                    style = MaterialTheme.typography.bodySmall,
                                    color = MaterialTheme.colorScheme.onSurfaceVariant
                                )
                            }
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
            containerColor = StudioColors.CardBg
        ) {
            Column(
                modifier = Modifier
                    .fillMaxWidth()
                    .padding(horizontal = 20.dp, vertical = 10.dp)
                    .navigationBarsPadding(),
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
                            selected = (mins == 0 && sleepTimerMinutes == 0) ||
                                (mins > 0 && sleepTimerMinutes > 0 && sleepTimerMinutes <= mins),
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
                    onValueChange = { pinCode = it },
                    label = { Text("PIN (Optional)") },
                    singleLine = true,
                    modifier = Modifier.fillMaxWidth()
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
