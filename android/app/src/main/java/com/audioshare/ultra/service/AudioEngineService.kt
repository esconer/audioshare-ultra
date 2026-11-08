package com.audioshare.ultra.service

import android.app.*
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.media.AudioAttributes
import android.media.AudioFocusRequest
import android.media.AudioManager
import android.media.session.MediaSession
import android.media.session.PlaybackState
import android.net.wifi.WifiManager
import android.os.Build
import android.os.IBinder
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
            nightMode: Boolean
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
    private var audioManager: AudioManager? = null
    private var focusRequest: AudioFocusRequest? = null
    private var mediaSession: MediaSession? = null
    private var currentGainDb = 0.0f

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (intent?.action == "ACTION_STOP") {
            stopStreamAndService()
            return START_NOT_STICKY
        }

        val hostAddr = intent?.getStringExtra("HOST_ADDR") ?: "192.168.42.129:65530"
        val isUsb = hostAddr.startsWith("192.168.42.") || hostAddr.startsWith("127.0.0.1")
        val gainDb = intent?.getFloatExtra("GAIN_DB", 0.0f) ?: 0.0f
        val pin = intent?.getStringExtra("PIN") ?: ""
        currentGainDb = gainDb

        startForegroundNotification(hostAddr, isUsb)
        acquireRealtimeHardwareLocks()
        setupHardwareBluetoothMediaSession()
        requestAudioFocus()

        isStreaming = nativeStartClient(hostAddr, isUsb, gainDb, pin)
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
        val wifiMgr = applicationContext.getSystemService(Context.WIFI_SERVICE) as WifiManager
        val mode = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            WifiManager.WIFI_MODE_FULL_LOW_LATENCY
        } else {
            @Suppress("DEPRECATION")
            WifiManager.WIFI_MODE_FULL_HIGH_PERF
        }
        wifiLock = wifiMgr.createWifiLock(mode, "AudioShareUltra::LowLatencyWifi").apply {
            setReferenceCounted(false)
            if (!isHeld) acquire()
        }

        val powerMgr = getSystemService(Context.POWER_SERVICE) as PowerManager
        wakeLock = powerMgr.newWakeLock(
            PowerManager.PARTIAL_WAKE_LOCK,
            "AudioShareUltra::RealtimeWakeLock"
        ).apply {
            setReferenceCounted(false)
            if (!isHeld) acquire()
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

        val badge = if (isUsb) "USB Lossless (<11ms)" else "Wi-Fi Lossless + FEC"
        val notification = NotificationCompat.Builder(this, channelId)
            .setContentTitle("AudioShare Ultra • $badge")
            .setContentText("Connected to $hostAddr")
            .setSmallIcon(android.R.drawable.ic_lock_silent_mode_off)
            .setOngoing(true)
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
        mediaSession?.isActive = false
        mediaSession?.release()
        mediaSession = null
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
