package com.audioshare.ultra.service

import android.app.*
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.content.pm.ServiceInfo
import android.media.AudioAttributes
import android.media.AudioFocusRequest
import android.media.AudioManager
import android.media.session.MediaSession
import android.media.session.PlaybackState
import android.net.wifi.WifiManager
import android.os.Build
import android.os.IBinder
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
        external fun nativeStartClient(hostAddr: String, isUsb: Boolean, gainDb: Float, pin: String): Boolean

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
            gamingLock: Boolean
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
    private var currentGainDb = 0.0f
    private var isLocallyMuted: Boolean = false
    private var currentHostAddr: String = ""
    private var currentIsUsb: Boolean = false
    private var noisyReceiverRegistered: Boolean = false

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
        if (intent?.action == "ACTION_START_CAST_GUARD") {
            // Standalone Mic / Party Host Mode: Hold ForegroundService + Low-Latency WifiLock + WakeLock
            // so Android never kills the microphone or Party Cast when the screen turns off!
            val label = intent.getStringExtra("CAST_LABEL") ?: "Mic / Party Broadcast"
            currentHostAddr = label
            currentIsUsb = false
            startForegroundNotification(label, false)
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
        currentGainDb = gainDb
        currentHostAddr = hostAddr
        currentIsUsb = isUsb
        isLocallyMuted = false

        startForegroundNotification(hostAddr, isUsb)
        acquireRealtimeHardwareLocks()
        setupHardwareBluetoothMediaSession()
        requestAudioFocus()
        if (!noisyReceiverRegistered) {
            try {
                registerReceiver(
                    becomingNoisyReceiver,
                    IntentFilter(AudioManager.ACTION_AUDIO_BECOMING_NOISY)
                )
                noisyReceiverRegistered = true
            } catch (_: Exception) {}
        }

        isStreaming = nativeStartClient(hostAddr, isUsb, gainDb, pin)
        // Restore saved Studio EQ, Spatial Role, Crossfeed, Night Mode & Gaming Lock automatically!
        nativeConfigureDsp(
            prefs.getFloat("bass_db", 0.0f),
            prefs.getFloat("vocal_db", 0.0f),
            prefs.getFloat("treble_db", 0.0f),
            prefs.getInt("channel_role", 0),
            prefs.getBoolean("crossfeed", false),
            prefs.getBoolean("night_mode", false),
            prefs.getBoolean("gaming_lock", false)
        )
        // Restore the compressed-codec backend selection on every stream
        // start: 20 = pure-Rust VBR (default), 21 = bundled Opus RLD.
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

    private fun requestAudioFocus() {
        val am = getSystemService(Context.AUDIO_SERVICE) as AudioManager
        // Reconnects call onStartCommand again; abandon the previous request
        // instead of overwriting it and leaking focus ownership.
        focusRequest?.let { audioManager?.abandonAudioFocusRequest(it) }
        audioManager = am
        val request = AudioFocusRequest.Builder(AudioManager.AUDIOFOCUS_GAIN)
            .setAudioAttributes(
                AudioAttributes.Builder()
                    .setUsage(AudioAttributes.USAGE_MEDIA)
                    .setContentType(AudioAttributes.CONTENT_TYPE_MUSIC)
                    .build()
            )
            .setOnAudioFocusChangeListener(this)
            .build()
        focusRequest = request
        am.requestAudioFocus(request)
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

    private fun startForegroundNotification(hostAddr: String, isUsb: Boolean) {
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
            // Include the microphone type only when RECORD_AUDIO is granted;
            // otherwise startForeground() would throw for the mic type.
            val micGranted = checkSelfPermission(android.Manifest.permission.RECORD_AUDIO) ==
                android.content.pm.PackageManager.PERMISSION_GRANTED
            val type = if (micGranted) {
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
        audioManager = null
        nativeStopPhoneMic()
        nativeStopClient()
        isStreaming = false
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
