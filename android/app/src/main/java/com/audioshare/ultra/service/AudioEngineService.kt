package com.audioshare.ultra.service

import android.app.*
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.content.pm.ServiceInfo
import android.media.AudioAttributes
import android.media.AudioDeviceCallback
import android.media.AudioDeviceInfo
import android.media.AudioFocusRequest
import android.media.AudioManager
import android.media.session.MediaSession
import android.media.session.PlaybackState
import android.net.wifi.WifiManager
import android.os.Build
import android.os.Handler
import android.os.IBinder
import android.os.Looper
import android.os.PerformanceHintManager
import android.os.PowerManager
import androidx.core.app.NotificationCompat

class AudioEngineService : Service(), AudioManager.OnAudioFocusChangeListener {

    companion object {
        init {
            System.loadLibrary("audioshare_jni")
        }

        @Volatile
        var isStreaming: Boolean = false
            private set

        @JvmStatic
        external fun nativeStartClient(
            hostAddr: String,
            isUsb: Boolean,
            gainDb: Float,
            pin: String,
            dolbyEffects: Boolean
        ): Boolean

        @JvmStatic
        external fun nativeStopClient()

        @JvmStatic
        external fun nativeSetGainDb(gainDb: Float)

        @JvmStatic
        external fun nativeGetTelemetry(): LongArray

        @JvmStatic
        external fun nativeReportBluetoothDelayMs(delayMs: Int)

        @JvmStatic
        external fun nativeSendMediaCommand(cmdCode: Int)

        @JvmStatic
        external fun nativeConfigureDsp(
            bassDb: Float,
            presDb: Float,
            trebDb: Float,
            role: Int,
            crossfeed: Boolean,
            nightMode: Boolean,
            gamingLock: Boolean,
            dolbyEffects: Boolean
        )

        @JvmStatic
        external fun nativePushCapturedAudio(pcmFloat: FloatArray)

        @JvmStatic
        external fun nativeStartPhoneMic(hostIp: String, useHardwareMic: Boolean): Boolean

        @JvmStatic
        external fun nativeStopPhoneMic()

        @JvmStatic
        external fun nativeGetDiscoveredHosts(): String
    }

    private var wifiLock: WifiManager.WifiLock? = null
    private var wakeLock: PowerManager.WakeLock? = null
    private var perfHintSession: PerformanceHintManager.Session? = null
    private var audioManager: AudioManager? = null
    private var focusRequest: AudioFocusRequest? = null
    private var mediaSession: MediaSession? = null
    private var audioDeviceCallback: AudioDeviceCallback? = null
    private var currentGainDb = 0.0f
    private var isLocallyMuted: Boolean = false
    private var currentHostAddr: String = ""
    private var currentIsUsb: Boolean = false
    private var noisyReceiverRegistered: Boolean = false
    // Raw low-latency output is the default. `true` opts into the shared
    // mixer so OEM post-processing (Dolby Atmos, Dirac, system EQ) applies.
    private var dolbyEffects: Boolean = false
    // Only microphone / internal-audio capture may declare the MICROPHONE
    // foreground-service type. Plain playback must never request it.
    private var micCaptureActive: Boolean = false
    private var foregroundStarted: Boolean = false

    // Auto-mute if wired headphones are unplugged or Bluetooth earbuds disconnect unexpectedly
    private val becomingNoisyReceiver = object : BroadcastReceiver() {
        override fun onReceive(context: Context?, intent: Intent?) {
            if (intent?.action == AudioManager.ACTION_AUDIO_BECOMING_NOISY) {
                isLocallyMuted = true
                nativeSetGainDb(-96.0f)
                startForegroundNotification(currentHostAddr, currentIsUsb)
            }
        }
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (intent?.action == "ACTION_STOP") {
            stopStreamAndService()
            return START_NOT_STICKY
        }
        if (intent?.action == "ACTION_SET_DOLBY") {
            val prefs = getSharedPreferences("audioshare_ultra_prefs", Context.MODE_PRIVATE)
            dolbyEffects = intent.getBooleanExtra("DOLBY_ENABLED", dolbyEffects)
            prefs.edit().putBoolean("dolby_fx", dolbyEffects).apply()
            // OEM Dolby daemons watch the focus attributes, so the focus
            // request must match the AAudio stream profile. Re-requesting
            // also abandons the stale attributes from the previous mode.
            requestAudioFocus(dolbyEffects)
            // The routed device query follows the focus attributes, so the
            // Bluetooth report must be refreshed after the mode change.
            reportBluetoothRoute()
            applyDspFromPrefs()
            if (foregroundStarted) {
                startForegroundNotification(currentHostAddr, currentIsUsb)
            }
            return START_STICKY
        }
        if (intent?.action == "ACTION_SET_MIC_CAPTURE") {
            micCaptureActive = intent.getBooleanExtra("MIC_ACTIVE", false)
            if (foregroundStarted) {
                startForegroundNotification(currentHostAddr, currentIsUsb)
            }
            return START_STICKY
        }
        if (intent?.action == "ACTION_START_CAST_GUARD") {
            // Standalone Mic / Party Host Mode: Hold ForegroundService + Low-Latency WifiLock + WakeLock
            // so Android never kills the microphone or Party Cast when the screen turns off!
            val label = intent.getStringExtra("CAST_LABEL") ?: "Mic / Party Broadcast"
            currentHostAddr = label
            currentIsUsb = false
            micCaptureActive = true
            startForegroundNotification(label, false, includeMicType = true)
            acquireRealtimeHardwareLocks()
            return START_STICKY
        }
        if (intent?.action == "ACTION_TOGGLE_MUTE") {
            val prefs = getSharedPreferences("audioshare_ultra_prefs", Context.MODE_PRIVATE)
            val savedGain = prefs.getFloat("gain_db", 0.0f)
            isLocallyMuted = !isLocallyMuted
            nativeSetGainDb(if (isLocallyMuted) -96.0f else savedGain)
            startForegroundNotification(currentHostAddr, currentIsUsb)
            return START_STICKY
        }

        val prefs = getSharedPreferences("audioshare_ultra_prefs", Context.MODE_PRIVATE)
        val hostAddr = intent?.getStringExtra("HOST_ADDR")
            ?: prefs.getString("last_host_addr", "192.168.42.129:65530")
            ?: "192.168.42.129:65530"
        // `getBooleanExtra(..., false)` returns false, not null, when the
        // extra is absent, so an Elvis fallback would never run. Check for the
        // extra explicitly so manual entry, QR scans and the quick-settings
        // tile still get USB auto-detection.
        val isUsb = intent?.let {
            if (it.hasExtra("IS_USB")) {
                it.getBooleanExtra("IS_USB", false)
            } else {
                isUsbHost(hostAddr)
            }
        } ?: isUsbHost(hostAddr)
        val gainDb = intent?.getFloatExtra("GAIN_DB", prefs.getFloat("gain_db", 0.0f)) ?: 0.0f
        val pin = intent?.getStringExtra("PIN") ?: prefs.getString("pin_code", "") ?: ""
        dolbyEffects = prefs.getBoolean("dolby_fx", false)
        currentGainDb = gainDb
        currentHostAddr = hostAddr
        currentIsUsb = isUsb
        isLocallyMuted = false

        startForegroundNotification(hostAddr, isUsb)
        acquireRealtimeHardwareLocks()
        setupHardwareBluetoothMediaSession()
        requestAudioFocus(dolbyEffects)
        startBluetoothRouteMonitoring()
        if (!noisyReceiverRegistered) {
            try {
                registerReceiver(
                    becomingNoisyReceiver,
                    IntentFilter(AudioManager.ACTION_AUDIO_BECOMING_NOISY)
                )
                noisyReceiverRegistered = true
            } catch (_: Exception) {}
        }

        isStreaming = nativeStartClient(hostAddr, isUsb, gainDb, pin, dolbyEffects)
        // Restore saved Studio EQ, Spatial Role, Crossfeed, Night Mode, Gaming
        // Lock and the Dolby/system-effects preference automatically!
        applyDspFromPrefs()
        // Restore both the saved Host Quality Mode (1..=5) and the compressed
        // codec backend (20 = Rust VBR, 21 = Opus RLD) on every stream start.
        // The native client keeps these in separate pending slots, so the two
        // calls cannot overwrite each other before the RX thread sends them.
        val savedQuality = prefs.getInt("quality_mode", 0).coerceIn(0, 4)
        nativeSendMediaCommand(savedQuality + 1)
        // Restore the compressed bitrate target (30..33 = 96/160/256/320 kbps)
        // in its own slot as well.
        val savedBitrateCmd = when (prefs.getInt("target_bitrate_kbps", 256)) {
            96 -> 30
            160 -> 31
            320 -> 33
            else -> 32
        }
        nativeSendMediaCommand(savedBitrateCmd)
        nativeSendMediaCommand(if (prefs.getBoolean("use_opus", false)) 21 else 20)
        return START_STICKY
    }

    private fun setupHardwareBluetoothMediaSession() {
        if (mediaSession != null) return
        mediaSession = MediaSession(this, "AudioShareUltraMediaSession").apply {
            setCallback(object : MediaSession.Callback() {
                override fun onPlay() { nativeSendMediaCommand(10) }
                override fun onPause() { nativeSendMediaCommand(10) }
                override fun onSkipToNext() { nativeSendMediaCommand(11) }
                override fun onSkipToPrevious() { nativeSendMediaCommand(12) }
            })
            setPlaybackState(
                PlaybackState.Builder()
                    .setActions(
                        PlaybackState.ACTION_PLAY or
                            PlaybackState.ACTION_PAUSE or
                            PlaybackState.ACTION_PLAY_PAUSE or
                            PlaybackState.ACTION_SKIP_TO_NEXT or
                            PlaybackState.ACTION_SKIP_TO_PREVIOUS
                    )
                    .setState(PlaybackState.STATE_PLAYING, 0L, 1.0f)
                    .build()
            )
            isActive = true
        }
    }

    /// Report the active Bluetooth route independently of the Compose UI.
    ///
    /// `MainScreen` polls the same route while it is visible, but playback can
    /// be started from the Quick Settings tile, the screen can be off, or
    /// another app can be in front. `AudioDeviceCallback` fires on every route
    /// add/remove, so the 145 ms A2DP allowance stays correct without the UI.
    private fun startBluetoothRouteMonitoring() {
        if (audioDeviceCallback == null) {
            val callback = object : AudioDeviceCallback() {
                override fun onAudioDevicesAdded(addedDevices: Array<out AudioDeviceInfo>) {
                    reportBluetoothRoute()
                }

                override fun onAudioDevicesRemoved(removedDevices: Array<out AudioDeviceInfo>) {
                    reportBluetoothRoute()
                }
            }
            try {
                val am = getSystemService(Context.AUDIO_SERVICE) as AudioManager
                am.registerAudioDeviceCallback(callback, Handler(Looper.getMainLooper()))
                audioDeviceCallback = callback
            } catch (_: Exception) {
            }
        }
        reportBluetoothRoute()
    }

    private fun reportBluetoothRoute() {
        val am = getSystemService(Context.AUDIO_SERVICE) as AudioManager
        val hasBt = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            // Match the attributes of the active AAudio stream: raw mode opens
            // with GAME/SONIFICATION, Dolby mode with MEDIA/MUSIC. Querying
            // the wrong usage can report the speaker while audio is routed to
            // Bluetooth.
            val attrs = AudioAttributes.Builder().apply {
                if (dolbyEffects) {
                    setUsage(AudioAttributes.USAGE_MEDIA)
                    setContentType(AudioAttributes.CONTENT_TYPE_MUSIC)
                } else {
                    setUsage(AudioAttributes.USAGE_GAME)
                    setContentType(AudioAttributes.CONTENT_TYPE_SONIFICATION)
                }
            }.build()
            val routedType = am.getAudioDevicesForAttributes(attrs).firstOrNull()?.type
            routedType == AudioDeviceInfo.TYPE_BLUETOOTH_A2DP ||
                routedType == AudioDeviceInfo.TYPE_BLUETOOTH_SCO ||
                routedType == AudioDeviceInfo.TYPE_BLE_HEADSET ||
                routedType == AudioDeviceInfo.TYPE_BLE_SPEAKER
        } else {
            // API < 31 has no per-attributes routing query; the deprecated
            // flags are the only way to distinguish a routed headset from a
            // paired-but-idle one.
            @Suppress("DEPRECATION")
            val a2dpRouted = am.isBluetoothA2dpOn
            @Suppress("DEPRECATION")
            val scoRouted = am.isBluetoothScoOn()
            a2dpRouted || scoRouted
        }
        // 0 means "no BT route"; never fake the A2DP allowance.
        nativeReportBluetoothDelayMs(if (hasBt) 145 else 0)
    }

    private fun requestAudioFocus(useDolby: Boolean) {
        val am = getSystemService(Context.AUDIO_SERVICE) as AudioManager
        // Reconnects call onStartCommand again; abandon the previous request
        // instead of overwriting it and leaking focus ownership.
        focusRequest?.let { audioManager?.abandonAudioFocusRequest(it) }
        audioManager = am
        // Raw mode uses game/sonification attributes so the platform and OEM
        // Dolby services do not treat the stream as media playback. Dolby mode
        // deliberately matches the shared-mixer AAudio profile.
        val attributes = AudioAttributes.Builder().apply {
            if (useDolby) {
                setUsage(AudioAttributes.USAGE_MEDIA)
                setContentType(AudioAttributes.CONTENT_TYPE_MUSIC)
            } else {
                setUsage(AudioAttributes.USAGE_GAME)
                setContentType(AudioAttributes.CONTENT_TYPE_SONIFICATION)
            }
        }.build()
        val request = AudioFocusRequest.Builder(AudioManager.AUDIOFOCUS_GAIN)
            .setAudioAttributes(attributes)
            .setOnAudioFocusChangeListener(this)
            .build()
        focusRequest = request
        am.requestAudioFocus(request)
    }

    private fun applyDspFromPrefs() {
        val prefs = getSharedPreferences("audioshare_ultra_prefs", Context.MODE_PRIVATE)
        AudioEngineService.nativeConfigureDsp(
            prefs.getFloat("bass_db", 0.0f),
            prefs.getFloat("vocal_db", 0.0f),
            prefs.getFloat("treble_db", 0.0f),
            prefs.getInt("channel_role", 0),
            prefs.getBoolean("crossfeed", false),
            prefs.getBoolean("night_mode", false),
            prefs.getBoolean("gaming_lock", false),
            prefs.getBoolean("dolby_fx", dolbyEffects)
        )
    }

    private fun acquireRealtimeHardwareLocks() {
        // onStartCommand can run again for a host switch or a cast-guard
        // restart. Reuse the existing locks instead of overwriting the only
        // reference to an acquired lock.
        if (wifiLock == null) {
            val wifiMgr = applicationContext.getSystemService(Context.WIFI_SERVICE) as WifiManager
            val mode = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
                WifiManager.WIFI_MODE_FULL_LOW_LATENCY
            } else {
                @Suppress("DEPRECATION")
                WifiManager.WIFI_MODE_FULL_HIGH_PERF
            }
            wifiLock = wifiMgr.createWifiLock(mode, "AudioShareUltra::LowLatencyWifi").apply {
                setReferenceCounted(false)
            }
        }
        wifiLock?.let { if (!it.isHeld) it.acquire() }

        if (wakeLock == null) {
            val powerMgr = getSystemService(Context.POWER_SERVICE) as PowerManager
            wakeLock = powerMgr.newWakeLock(
                PowerManager.PARTIAL_WAKE_LOCK,
                "AudioShareUltra::RealtimeWakeLock"
            ).apply {
                setReferenceCounted(false)
            }
        }
        wakeLock?.let { if (!it.isHeld) it.acquire() }

        // Android 12+ ADPF (Dynamic Performance Framework): Request 2.5ms real-time scheduler boost
        // to prevent ARM CPU cores from entering deep C3 sleep between Wi-Fi packets!
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            try {
                perfHintSession?.close()
            } catch (_: Exception) {
            }
            perfHintSession = null
            try {
                val hintMgr = getSystemService(PerformanceHintManager::class.java)
                val tids = intArrayOf(android.os.Process.myTid())
                perfHintSession = hintMgr?.createHintSession(tids, 2_500_000L) // 2.5ms deadline
            } catch (_: Exception) {
            }
        }
    }

    private fun isUsbHost(hostAddr: String): Boolean {
        if (hostAddr.startsWith("127.0.0.1") ||
            hostAddr.startsWith("192.168.42.") ||
            hostAddr.startsWith("192.168.137.")
        ) {
            return true
        }
        val hostIp = hostAddr.substringBefore(':')
        return try {
            val ifaces = java.net.NetworkInterface.getNetworkInterfaces() ?: return false
            for (iface in ifaces) {
                val name = iface.name.lowercase()
                val isUsbName = name.contains("rndis") ||
                    name.contains("usb") ||
                    name.contains("ncm") ||
                    name.startsWith("enx") ||
                    (name.startsWith("enp") && name.contains('u'))
                if (isUsbName && iface.isUp) {
                    for (addr in iface.interfaceAddresses) {
                        val ip = addr.address?.hostAddress ?: continue
                        val prefix = ip.substringBeforeLast('.')
                        if (hostIp.startsWith("$prefix.")) {
                            return true
                        }
                    }
                }
            }
            false
        } catch (_: Exception) {
            false
        }
    }

    private fun startForegroundNotification(
        hostAddr: String,
        isUsb: Boolean,
        includeMicType: Boolean = micCaptureActive
    ) {
        val channelId = "audioshare_ultra_stream"
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            val chan = NotificationChannel(
                channelId,
                "AudioShare Ultra Real-Time Stream",
                NotificationManager.IMPORTANCE_LOW
            )
            getSystemService(NotificationManager::class.java).createNotificationChannel(chan)
        }

        val stopIntent = Intent(this, AudioEngineService::class.java).apply {
            action = "ACTION_STOP"
        }
        val stopPending = PendingIntent.getService(
            this, 0, stopIntent,
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE
        )
        val muteIntent = Intent(this, AudioEngineService::class.java).apply {
            action = "ACTION_TOGGLE_MUTE"
        }
        val mutePending = PendingIntent.getService(
            this, 1, muteIntent,
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE
        )

        val badge = if (isUsb) "USB Lossless (<11ms)" else "Wi-Fi Lossless + FEC"
        val statusText = if (isLocallyMuted) "MUTED • $hostAddr" else "Streaming • $hostAddr"
        val notification = NotificationCompat.Builder(this, channelId)
            .setContentTitle("AudioShare Ultra • $badge")
            .setContentText(statusText)
            .setSmallIcon(android.R.drawable.ic_lock_silent_mode_off)
            .setOngoing(true)
            .addAction(android.R.drawable.ic_lock_silent_mode, if (isLocallyMuted) "Unmute" else "Mute", mutePending)
            .addAction(android.R.drawable.ic_media_pause, "Disconnect", stopPending)
            .build()

        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            // Declare MICROPHONE only while a mic or internal-audio capture
            // actually runs and RECORD_AUDIO is granted. Normal playback
            // intentionally omits it, so the OS does not treat a plain
            // music stream as microphone use.
            val micGranted = checkSelfPermission(android.Manifest.permission.RECORD_AUDIO) ==
                android.content.pm.PackageManager.PERMISSION_GRANTED
            val type = if (includeMicType && micGranted) {
                ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PLAYBACK or
                    ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE
            } else {
                ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PLAYBACK
            }
            startForeground(101, notification, type)
        } else if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            startForeground(101, notification, ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PLAYBACK)
        } else {
            startForeground(101, notification)
        }
        foregroundStarted = true
    }

    override fun onAudioFocusChange(focusChange: Int) {
        when (focusChange) {
            AudioManager.AUDIOFOCUS_LOSS_TRANSIENT,
            AudioManager.AUDIOFOCUS_LOSS -> nativeSetGainDb(-96.0f) // Auto-mute during phone call
            AudioManager.AUDIOFOCUS_LOSS_TRANSIENT_CAN_DUCK ->
                nativeSetGainDb((currentGainDb - 12.0f).coerceAtLeast(-96.0f))
            AudioManager.AUDIOFOCUS_GAIN -> nativeSetGainDb(currentGainDb)
        }
    }

    private fun stopStreamAndService() {
        focusRequest?.let { audioManager?.abandonAudioFocusRequest(it) }
        focusRequest = null
        audioDeviceCallback?.let { callback ->
            try {
                audioManager?.unregisterAudioDeviceCallback(callback)
            } catch (_: Exception) {
            }
        }
        audioDeviceCallback = null
        audioManager = null
        nativeStopPhoneMic()
        nativeStopClient()
        isStreaming = false
        micCaptureActive = false
        foregroundStarted = false
        if (noisyReceiverRegistered) {
            try {
                unregisterReceiver(becomingNoisyReceiver)
            } catch (_: Exception) {}
            noisyReceiverRegistered = false
        }
        mediaSession?.isActive = false
        mediaSession?.release()
        mediaSession = null
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            try { perfHintSession?.close() } catch (_: Exception) {}
            perfHintSession = null
        }
        wifiLock?.let { if (it.isHeld) it.release() }
        wakeLock?.let { if (it.isHeld) it.release() }
        stopForeground(STOP_FOREGROUND_REMOVE)
        stopSelf()
    }

    override fun onDestroy() {
        stopStreamAndService()
        super.onDestroy()
    }

    override fun onBind(intent: Intent?): IBinder? = null
}
