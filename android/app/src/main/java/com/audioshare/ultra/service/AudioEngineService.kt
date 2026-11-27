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
import android.media.MediaMetadata
import android.media.projection.MediaProjection
import android.media.projection.MediaProjectionManager
import android.media.session.MediaSession
import android.media.session.PlaybackState
import android.net.wifi.WifiManager
import android.os.Build
import android.os.Handler
import android.os.IBinder
import android.os.Looper
import android.os.PerformanceHintManager
import android.os.PowerManager
import android.util.Log
import android.view.KeyEvent
import androidx.core.app.NotificationCompat
import com.audioshare.ultra.MainActivity
import com.audioshare.ultra.R
import com.audioshare.ultra.ui.HostPinStore
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow

class AudioEngineService : Service(), AudioManager.OnAudioFocusChangeListener {

    companion object {
        init {
            System.loadLibrary("audioshare_jni")
        }

        @Volatile
        var isStreaming: Boolean = false
            private set

        // Party Mode (internal-audio cast) is owned by this service, not the
        // Compose UI: the MediaProjection consent token is one-shot and must
        // be redeemed only after startForeground(...MEDIA_PROJECTION).
        const val ACTION_START_PARTY_CAST = "ACTION_START_PARTY_CAST"
        const val ACTION_STOP_PARTY_CAST = "ACTION_STOP_PARTY_CAST"
        const val ACTION_PARTY_CAST_STATE = "ACTION_PARTY_CAST_STATE"
        const val ACTION_STOP_CLIENT_ONLY = "ACTION_STOP_CLIENT_ONLY"
        const val EXTRA_PARTY_CAST_ACTIVE = "PARTY_CAST_ACTIVE"
        const val EXTRA_PARTY_CAST_ERROR = "PARTY_CAST_ERROR"
        const val ACTION_START_PHONE_MIC = "ACTION_START_PHONE_MIC"
        const val ACTION_STOP_PHONE_MIC = "ACTION_STOP_PHONE_MIC"
        const val ACTION_TOGGLE_MIC_MUTE = "ACTION_TOGGLE_MIC_MUTE"
        const val ACTION_PHONE_MIC_STATE = "ACTION_PHONE_MIC_STATE"
        const val EXTRA_PHONE_MIC_ACTIVE = "PHONE_MIC_ACTIVE"
        const val ACTION_SET_BT_OFFSET = "ACTION_SET_BT_OFFSET"
        const val ACTION_CLEAR_LOCAL_MUTE = "ACTION_CLEAR_LOCAL_MUTE"
        // Service-owned sleep timer: the countdown lives here so screen-off and
        // Activity recreation can no longer kill it.
        const val ACTION_SET_SLEEP_TIMER = "ACTION_SET_SLEEP_TIMER"
        const val ACTION_SLEEP_TIMER_STATE = "ACTION_SLEEP_TIMER_STATE"
        const val EXTRA_SLEEP_REMAINING_MINS = "SLEEP_REMAINING_MINS"
        // Stream connect result is broadcast so the UI can show a real error
        // Snackbar instead of optimistically claiming a successful connection.
        const val ACTION_STREAM_STATE = "ACTION_STREAM_STATE"
        const val EXTRA_STREAM_ACTIVE = "STREAM_ACTIVE"
        const val EXTRA_STREAM_ERROR = "STREAM_ERROR"
        // Party Host actions are handled by the service so they work from the
        // foreground notification even when the Compose UI is not running.
        const val ACTION_TOGGLE_PARTY_MUTE = "ACTION_TOGGLE_PARTY_MUTE"
        const val ACTION_SET_PARTY_LOCAL_SPEAKER_MUTE = "ACTION_SET_PARTY_LOCAL_SPEAKER_MUTE"
        const val ACTION_DEEP_LINK_HOST = "ACTION_DEEP_LINK_HOST"
        const val EXTRA_PHONE_MIC_ERROR = "PHONE_MIC_ERROR"

        @Volatile
        var isPartyCasting: Boolean = false
            private set

        @Volatile
        var activePartyPin: String = ""
            private set

        @Volatile
        var isPhoneMicActive: Boolean = false
            private set

        @Volatile
        var sleepRemainingMinutes: Int = 0
            private set

        @Volatile
        var isLocallyMutedState: Boolean = false
            private set
        private val _isLocallyMutedFlow = MutableStateFlow(false)
        val isLocallyMutedFlow: StateFlow<Boolean> = _isLocallyMutedFlow.asStateFlow()

        @Volatile
        var isMicMutedState: Boolean = false
            private set
        private val _isMicMutedFlow = MutableStateFlow(false)
        val isMicMutedFlow: StateFlow<Boolean> = _isMicMutedFlow.asStateFlow()

        // Process-wide single-thread executor and generation counters matching
        // the process-wide Rust statics (`GLOBAL_ANDROID_CLIENT` /
        // `GLOBAL_PHONE_MIC_TX`). Keeping these in the companion object ensures
        // a rapid Service destroy/recreate (e.g. Disconnect -> Reconnect) can
        // NEVER race Instance #1's async teardown against Instance #2's start.
        private val engineExecutor = java.util.concurrent.Executors.newSingleThreadExecutor()
        private val connectGen = java.util.concurrent.atomic.AtomicLong(0)
        private val micGen = java.util.concurrent.atomic.AtomicLong(0)

        @JvmStatic
        external fun nativeStartClient(
            hostAddr: String,
            isUsb: Boolean,
            gainDb: Float,
            pin: String,
            dolbyEffects: Boolean
        ): Boolean

        @JvmStatic
        external fun nativeStartClientConfigured(
            hostAddr: String,
            isUsb: Boolean,
            gainDb: Float,
            pin: String,
            dolbyEffects: Boolean,
            bassDb: Float,
            presDb: Float,
            trebDb: Float,
            role: Int,
            crossfeed: Boolean,
            nightMode: Boolean,
            gamingLock: Boolean
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
        external fun nativeRequestExclusiveMmapReclaim()

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
        external fun nativeStartPhoneMic(hostIp: String, useHardwareMic: Boolean, pin: String): Boolean

        @JvmStatic
        external fun nativeStopPhoneMic()

        @JvmStatic
        external fun nativeSetPartyCastMuted(muted: Boolean)

        @JvmStatic
        external fun nativeSetPartyCodecConfig(codecProfile: Int, bitrateKbps: Int)

        @JvmStatic
        external fun nativeTakePartyMediaCommand(): Int

        @JvmStatic
        external fun nativeGetDiscoveredHosts(): String

        @JvmStatic
        external fun nativeGetRealtimeTids(): IntArray

        @JvmStatic
        external fun nativeGetDiagnostics(): String?

        @JvmStatic
        external fun nativeGetAuthFailures(): Long

        @JvmStatic
        external fun nativeLogEvent(level: Int, component: String, message: String)

        @JvmStatic
        external fun nativeSetScannerPaused(paused: Boolean)

        @JvmStatic
        external fun nativeInstallCrashHook(crashLogPath: String)
    }

    private var wifiLock: WifiManager.WifiLock? = null

    override fun onCreate() {
        super.onCreate()
        runCatching {
            val crashPath = java.io.File(filesDir, "audioshare-ultra-crash.log").absolutePath
            nativeInstallCrashHook(crashPath)
        }
    }

    private var wakeLock: PowerManager.WakeLock? = null
    private var perfHintSession: PerformanceHintManager.Session? = null
    private var audioManager: AudioManager? = null
    private var focusRequest: AudioFocusRequest? = null
    private var mediaSession: MediaSession? = null
    private var audioDeviceCallback: AudioDeviceCallback? = null
    private var currentGainDb = 0.0f
    private var isLocallyMuted: Boolean = false
        set(value) {
            field = value
            isLocallyMutedState = value
            _isLocallyMutedFlow.value = value
        }
    // Last value handed to native A/V sync. Device callbacks can re-report the
    // same route repeatedly; log only real transitions to keep the report useful.
    private var lastReportedBtDelayMs: Int = -1
    private var lastReportedBtRouteMode: String = ""
    private var currentHostAddr: String = ""
    private var currentIsUsb: Boolean = false
    private var noisyReceiverRegistered: Boolean = false
    private var remoteMediaPlaying: Boolean = true
    // Raw low-latency output is the default. `true` opts into the shared
    // mixer so OEM post-processing (Dolby Atmos, Dirac, system EQ) applies.
    private var dolbyEffects: Boolean = false
    // Tracks whether audio focus was interrupted (call / notification / loss)
    // so regaining focus triggers one bounded Exclusive MMAP reclaim attempt
    // if AAudio was kicked down to the shared mixer.
    private var lostAudioFocus: Boolean = false
    // Only microphone / internal-audio capture may declare the MICROPHONE
    // foreground-service type. Plain playback must never request it.
    private var micCaptureActive: Boolean = false
    // Internal-audio playback capture is a mediaProjection foreground
    // service, not a microphone one.
    private var projectionActive: Boolean = false
    private var activeCaster: PhoneAudioCaster? = null
    private var foregroundStarted: Boolean = false
    // Party-only state owned here (not in Compose) so a screen-off / background
    // host never leaves the phone speaker stuck at volume 0 and peer media keys
    // keep working. Default local-speaker mute is ON (party_mute_local = true).
    private var isPartyCastMuted: Boolean = false
    private var isMicMuted: Boolean = false
        set(value) {
            field = value
            isMicMutedState = value
            _isMicMutedFlow.value = value
        }
    private var serviceDestroyed: Boolean = false
    private var serviceStopped: Boolean = false
    private var savedPartyMusicVolume: Int = -1
    private val partyMediaHandler = Handler(Looper.getMainLooper())
    // Polls peer remote media commands (10..12) in the Service so friends can
    // control playback even when the Party Host screen is off.
    private val partyMediaPollRunnable = object : Runnable {
        override fun run() {
            if (!isPartyCasting) return
            val cmd = runCatching { nativeTakePartyMediaCommand() }.getOrDefault(0)
            if (cmd in 10..12) {
                val am = getSystemService(Context.AUDIO_SERVICE) as AudioManager
                val keyCode = when (cmd) {
                    10 -> KeyEvent.KEYCODE_MEDIA_PLAY_PAUSE
                    11 -> KeyEvent.KEYCODE_MEDIA_NEXT
                    else -> KeyEvent.KEYCODE_MEDIA_PREVIOUS
                }
                am.dispatchMediaKeyEvent(KeyEvent(KeyEvent.ACTION_DOWN, keyCode))
                am.dispatchMediaKeyEvent(KeyEvent(KeyEvent.ACTION_UP, keyCode))
            }
            partyMediaHandler.postDelayed(this, 250L)
        }
    }
    // The native RX/TX threads publish their TIDs shortly after start, so the
    // first ADPF query can race them. Refresh once immediately and once on the
    // next half-second without accumulating delayed callbacks.
    private val adpfHandler = Handler(Looper.getMainLooper())
    private val adpfRefreshRunnable = Runnable { refreshAdpfHintSession() }
    private val sleepHandler = Handler(Looper.getMainLooper())
    private val mainHandler = Handler(Looper.getMainLooper())
    private val sleepTickRunnable = object : Runnable {
        override fun run() {
            if (sleepRemainingMinutes <= 1) {
                broadcastSleepTimer(0)
                stopStreamAndService()
            } else {
                broadcastSleepTimer(sleepRemainingMinutes - 1)
                sleepHandler.postDelayed(this, 60_000L)
            }
        }
    }

    // Host-silence watchdog. The old 300 s auto-disconnect lived in the Compose
    // UI, which cannot poll once the screen is off. Keep it here so it survives
    // rotation and backgrounding: tick every 15 s and stop cleanly once the host
    // telemetry reports >= 300 s of silence, but only for a plain listening
    // session (no party cast, no phone mic). Native telemetry index 6 is the
    // existing host-silence seconds counter.
    private val silenceHandler = Handler(Looper.getMainLooper())
    private val silenceWatchdogRunnable = object : Runnable {
        override fun run() {
            if (!isStreaming && !isPartyCasting && !isPhoneMicActive) return
            if (isStreaming && !isPartyCasting && !isPhoneMicActive) {
                val silenceSec = runCatching { nativeGetTelemetry().getOrElse(6) { 0L } }
                    .getOrDefault(0L)
                if (silenceSec >= 300L) {
                    ensureForegroundBeforeStop()
                    stopStreamAndService()
                    return
                }
            }
            silenceHandler.postDelayed(this, 15_000L)
        }
    }

    private fun scheduleSilenceWatchdog() {
        silenceHandler.removeCallbacks(silenceWatchdogRunnable)
        silenceHandler.postDelayed(silenceWatchdogRunnable, 15_000L)
    }

    private fun stopSilenceWatchdog() {
        silenceHandler.removeCallbacks(silenceWatchdogRunnable)
    }

    // Auto-mute if wired headphones are unplugged or Bluetooth earbuds disconnect unexpectedly
    private val becomingNoisyReceiver = object : BroadcastReceiver() {
        override fun onReceive(context: Context?, intent: Intent?) {
            if (intent?.action == AudioManager.ACTION_AUDIO_BECOMING_NOISY) {
                runCatching {
                    nativeLogEvent(1, "audio-noisy", "Audio becoming noisy - output auto-muted")
                }
                isLocallyMuted = true
                nativeSetGainDb(-96.0f)
                startForegroundNotification(currentHostAddr, currentIsUsb)
            }
        }
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        // A sticky restart delivers a null intent. If no stream is alive there
        // is nothing to reconnect, so tearing down here avoids falling through
        // to the auto-connect path with the default host.
        if (intent == null) {
            if (!isStreaming) {
                stopSelf()
                return START_NOT_STICKY
            }
        }
        if (intent?.action == "ACTION_STOP") {
            ensureForegroundBeforeStop()
            stopStreamAndService()
            return START_NOT_STICKY
        }
        serviceStopped = false
        if (intent?.action == ACTION_STOP_CLIENT_ONLY) {
            val keepAlive = isPartyCasting || micCaptureActive
            val gen = connectGen.incrementAndGet()
            // Release PC-playback-only OS hooks immediately (MediaSession,
            // AudioFocus, becomingNoisyReceiver, audioDeviceCallback) so an
            // active Party Mode session does not route peer media keys right
            // back into our own dead MediaSession.
            releaseClientPlaybackResources()
            isStreaming = false
            isLocallyMuted = false
            sleepHandler.removeCallbacks(sleepTickRunnable)
            stopSilenceWatchdog()
            broadcastSleepTimer(0)
            broadcastStreamState(false, null)
            runOnEngine {
                if (gen == connectGen.get()) {
                    nativeStopClient()
                }
                mainHandler.post {
                    if (serviceDestroyed || gen != connectGen.get()) return@post
                    if (!isPartyCasting && !micCaptureActive) {
                        stopStreamAndService()
                    } else {
                        startForegroundNotification(currentHostAddr, currentIsUsb)
                    }
                }
            }
            return if (keepAlive) START_STICKY else START_NOT_STICKY
        }
        if (intent?.action == "ACTION_SET_DOLBY") {
            val prefs = getSharedPreferences("audioshare_ultra_prefs", Context.MODE_PRIVATE)
            dolbyEffects = intent.getBooleanExtra("DOLBY_ENABLED", dolbyEffects)
            prefs.edit().putBoolean("dolby_fx", dolbyEffects).apply()
            // Reopen the AAudio stream BEFORE querying `reportBluetoothRoute()`
            // and refresh once more after Android's HAL settles (400 ms) so
            // `bluetooth_delay_ms` does not stay stuck on Samsung's 290 ms
            // normal-mixer Dolby delay after toggling Dolby OFF.
            applyDspFromPrefs()
            reportBluetoothRoute()
            mainHandler.postDelayed({
                if (!serviceDestroyed && isStreaming) reportBluetoothRoute()
            }, 400L)
            if (foregroundStarted) {
                startForegroundNotification(currentHostAddr, currentIsUsb)
            }
            return START_STICKY
        }
        if (intent?.action == ACTION_START_PARTY_CAST) {
            return startPartyCast(intent)
        }
        if (intent?.action == ACTION_STOP_PARTY_CAST) {
            return stopPartyCast()
        }
        if (intent?.action == ACTION_TOGGLE_PARTY_MUTE) {
            isPartyCastMuted = !isPartyCastMuted
            runCatching { nativeSetPartyCastMuted(isPartyCastMuted) }
            if (foregroundStarted) startForegroundNotification(currentHostAddr, currentIsUsb)
            return START_STICKY
        }
        if (intent?.action == ACTION_SET_PARTY_LOCAL_SPEAKER_MUTE) {
            applyLocalPartySpeakerMute(intent.getBooleanExtra("MUTE_LOCAL", false))
            return START_STICKY
        }
        if (intent?.action == ACTION_TOGGLE_MIC_MUTE) {
            if (micCaptureActive) {
                isMicMuted = !isMicMuted
                runCatching { nativeSetPartyCastMuted(isMicMuted) }
                if (foregroundStarted) {
                    startForegroundNotification(currentHostAddr, currentIsUsb, includeMicType = true)
                }
            }
            return START_STICKY
        }
        if (intent?.action == ACTION_START_PHONE_MIC) {
            // targetSdk 34 forbids declaring the MICROPHONE foreground-service
            // type without RECORD_AUDIO. Refuse before touching the native mic
            // so a denied permission never leaves an orphan FGS running.
            if (checkSelfPermission(android.Manifest.permission.RECORD_AUDIO) !=
                android.content.pm.PackageManager.PERMISSION_GRANTED
            ) {
                broadcastPhoneMicState(false, "Microphone permission is required to share phone mic")
                if (!isStreaming && !isPartyCasting) {
                    ensureForegroundBeforeStop()
                    stopStreamAndService()
                    return START_NOT_STICKY
                }
                return START_STICKY
            }
            val target = intent.getStringExtra("HOST_ADDR").orEmpty().trim()
            val pin = intent.getStringExtra("PIN").orEmpty().trim()
            if (!isStreaming) {
                currentHostAddr = target.ifEmpty { "PC Video-Call Mic" }
                currentIsUsb = isUsbHost(target)
            }
            // Phone mic always starts unmuted (decoupled from PC speaker mute).
            isMicMuted = false
            // targetSdk 34 requires the MICROPHONE foreground-service type to
            // be active BEFORE AAudio opens the input. Publish FGS synchronously
            // on main, then serialize native start on `engineExecutor` so it
            // never races `ACTION_STOP_PHONE_MIC` or blocks the UI thread.
            val gen = micGen.incrementAndGet()
            micCaptureActive = true
            startForegroundNotification(currentHostAddr, currentIsUsb, includeMicType = true)
            acquireRealtimeHardwareLocks()
            runOnEngine {
                if (gen != micGen.get()) return@runOnEngine
                val started = runCatching { nativeStartPhoneMic(target, true, pin) }.getOrDefault(false)
                if (started) {
                    runCatching { nativeSetPartyCastMuted(false) }
                }
                mainHandler.post {
                    if (serviceDestroyed || gen != micGen.get()) return@post
                    if (!started) {
                        micCaptureActive = false
                        broadcastPhoneMicState(
                            false,
                            "Unable to start phone mic for '$target' — select a valid PC host IP or USB first"
                        )
                        if (!isStreaming && !isPartyCasting) {
                            stopStreamAndService()
                        } else {
                            startForegroundNotification(
                                currentHostAddr,
                                currentIsUsb,
                                includeMicType = false
                            )
                        }
                        return@post
                    }
                    broadcastPhoneMicState(true)
                    startForegroundNotification(currentHostAddr, currentIsUsb, includeMicType = true)
                    refreshAdpfHintSession()
                    scheduleAdpfRefresh()
                }
            }
            // A mic-only session has no peer to reconnect to after process
            // death, so do not request a sticky restart for it.
            return if (isStreaming) START_STICKY else START_NOT_STICKY
        }
        if (intent?.action == ACTION_STOP_PHONE_MIC) {
            val keepAlive = isStreaming || isPartyCasting
            val gen = micGen.incrementAndGet()
            isMicMuted = false
            runOnEngine {
                if (gen == micGen.get()) {
                    nativeStopPhoneMic()
                }
                mainHandler.post {
                    if (serviceDestroyed || gen != micGen.get()) return@post
                    micCaptureActive = false
                    broadcastPhoneMicState(false)
                    if (!isStreaming && !isPartyCasting) {
                        stopStreamAndService()
                    } else {
                        startForegroundNotification(
                            currentHostAddr,
                            currentIsUsb,
                            includeMicType = false
                        )
                    }
                }
            }
            return if (keepAlive) START_STICKY else START_NOT_STICKY
        }
        if (intent?.action == ACTION_SET_BT_OFFSET) {
            reportBluetoothRoute()
            if (!isStreaming && !isPartyCasting && !micCaptureActive) {
                stopSelf()
                return START_NOT_STICKY
            }
            return START_STICKY
        }
        if (intent?.action == ACTION_CLEAR_LOCAL_MUTE) {
            // The UI's preamp slider or Unmute button restores PC speaker audio.
            // Re-claim audio focus too so future phone calls trigger focus loss.
            if (isLocallyMuted) {
                isLocallyMuted = false
                val prefs = getSharedPreferences("audioshare_ultra_prefs", Context.MODE_PRIVATE)
                val savedGain = prefs.getFloat("gain_db", currentGainDb)
                nativeSetGainDb(savedGain)
                if (isStreaming) {
                    requestAudioFocus(dolbyEffects)
                    if (lostAudioFocus && !dolbyEffects) {
                        lostAudioFocus = false
                        runCatching { nativeRequestExclusiveMmapReclaim() }
                    }
                }
                if (foregroundStarted) startForegroundNotification(currentHostAddr, currentIsUsb)
            }
            if (!isStreaming && !isPartyCasting && !micCaptureActive) {
                stopSelf()
                return START_NOT_STICKY
            }
            return START_STICKY
        }
        if (intent?.action == ACTION_SET_SLEEP_TIMER) {
            val mins = intent.getIntExtra("MINUTES", 0).coerceAtLeast(0)
            sleepHandler.removeCallbacks(sleepTickRunnable)
            broadcastSleepTimer(mins)
            if (mins > 0) {
                sleepHandler.postDelayed(sleepTickRunnable, 60_000L)
            } else if (!isStreaming && !isPartyCasting && !micCaptureActive) {
                stopSelf()
                return START_NOT_STICKY
            }
            return START_STICKY
        }
        if (intent?.action == "ACTION_TOGGLE_MUTE") {
            val prefs = getSharedPreferences("audioshare_ultra_prefs", Context.MODE_PRIVATE)
            val savedGain = prefs.getFloat("gain_db", 0.0f)
            isLocallyMuted = !isLocallyMuted
            nativeSetGainDb(if (isLocallyMuted) -96.0f else savedGain)
            if (!isLocallyMuted && isStreaming) {
                // Re-claim audio focus so the app that stole it pauses instead
                // of playing over us after the user unmutes.
                requestAudioFocus(dolbyEffects)
                if (lostAudioFocus && !dolbyEffects) {
                    lostAudioFocus = false
                    runCatching { nativeRequestExclusiveMmapReclaim() }
                }
            }
            startForegroundNotification(currentHostAddr, currentIsUsb)
            return START_STICKY
        }
        if (intent?.action == "ACTION_REMOTE_PLAY_PAUSE") {
            updateMediaSessionPlaybackState(!remoteMediaPlaying)
            nativeSendMediaCommand(10)
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
        val pin = intent?.getStringExtra("PIN") ?: HostPinStore.getPin(prefs, hostAddr)
        dolbyEffects = prefs.getBoolean("dolby_fx", false)
        currentGainDb = gainDb
        currentHostAddr = hostAddr
        currentIsUsb = isUsb
        isLocallyMuted = false

        setupHardwareBluetoothMediaSession()
        startForegroundNotification(hostAddr, isUsb)
        acquireRealtimeHardwareLocks()
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

        // Snapshot saved DSP / gaming-lock settings before spawning the native
        // RX thread so the very first feedback packet and audio frame already
        // carry the user's configuration atomically.
        val bassDb = prefs.getFloat("bass_db", 0.0f)
        val vocalDb = prefs.getFloat("vocal_db", 0.0f)
        val trebleDb = prefs.getFloat("treble_db", 0.0f)
        val channelRole = prefs.getInt("channel_role", 0)
        val crossfeed = prefs.getBoolean("crossfeed", false)
        val nightMode = prefs.getBoolean("night_mode", false)
        val gamingLock = prefs.getBoolean("gaming_lock", false)

        // Creating the native AAudio client blocks while the stream is
        // negotiated. The foreground notification is already up (synchronously,
        // on main), so the blocking work can safely move to the engine thread.
        val gen = connectGen.incrementAndGet()
        runOnEngine {
            val started = runCatching {
                nativeStartClientConfigured(
                    hostAddr,
                    isUsb,
                    gainDb,
                    pin,
                    dolbyEffects,
                    bassDb,
                    vocalDb,
                    trebleDb,
                    channelRole,
                    crossfeed,
                    nightMode,
                    gamingLock
                )
            }.getOrDefault(false)
            mainHandler.post { handleClientStartResult(started, hostAddr, gen) }
        }
        return START_STICKY
    }

    /** Called on the main looper after the blocking native client start finishes. */
    private fun handleClientStartResult(started: Boolean, hostAddr: String, gen: Long) {
        // A teardown or a newer connect may have happened while JNI was blocking.
        if (serviceDestroyed || gen != connectGen.get()) return
        isStreaming = started
        if (!started) {
            // Stop any previously-running ClientEngine (e.g. if the user tried
            // switching to an invalid host address while Party/Mic was active,
            // `android_start_stream` returned Err before stopping the old client).
            releaseClientPlaybackResources()
            runOnEngine {
                if (gen == connectGen.get()) {
                    nativeStopClient()
                }
            }
            if (!isPartyCasting && !micCaptureActive) {
                // stopStreamAndService() emits a clean false state; send the
                // error AFTER teardown so the Snackbar text is not overwritten.
                stopStreamAndService()
                broadcastStreamState(
                    false,
                    "Unable to connect to $hostAddr (check IP, firewall, or USB)"
                )
                return
            }
            startForegroundNotification(currentHostAddr, currentIsUsb)
            broadcastStreamState(
                false,
                "Unable to connect to $hostAddr (check IP, firewall, or USB)"
            )
            return
        }
        broadcastStreamState(true, null)
        // Refresh notification now that `isStreaming == true` so PC Stream
        // controls (Pause PC / Mute / Disconnect) appear even if Party/Mic was
        // already running before the stream connected.
        startForegroundNotification(currentHostAddr, currentIsUsb)
        scheduleSilenceWatchdog()
        // Re-apply the active Bluetooth route delay now that the native
        // ClientEngine exists, and refresh DSP prefs in case the user adjusted
        // controls while the AAudio open was blocking.
        reportBluetoothRoute()
        applyDspFromPrefs()
        refreshAdpfHintSession()
        scheduleAdpfRefresh()
    }

    private fun updateMediaSessionPlaybackState(playing: Boolean) {
        remoteMediaPlaying = playing
        mediaSession?.setPlaybackState(
            PlaybackState.Builder()
                .setActions(
                    PlaybackState.ACTION_PLAY or
                        PlaybackState.ACTION_PAUSE or
                        PlaybackState.ACTION_PLAY_PAUSE or
                        PlaybackState.ACTION_SKIP_TO_NEXT or
                        PlaybackState.ACTION_SKIP_TO_PREVIOUS
                )
                .setState(
                    if (playing) PlaybackState.STATE_PLAYING else PlaybackState.STATE_PAUSED,
                    0L,
                    1.0f
                )
                .build()
        )
    }

    private fun setupHardwareBluetoothMediaSession() {
        if (mediaSession != null) return
        mediaSession = MediaSession(this, "AudioShareUltraMediaSession").apply {
            setMetadata(
                MediaMetadata.Builder()
                    .putString(MediaMetadata.METADATA_KEY_TITLE, "AudioShare Ultra")
                    .putString(MediaMetadata.METADATA_KEY_ARTIST, "PC Audio")
                    .build()
            )
            setCallback(object : MediaSession.Callback() {
                override fun onPlay() {
                    updateMediaSessionPlaybackState(true)
                    nativeSendMediaCommand(10)
                    if (foregroundStarted) startForegroundNotification(currentHostAddr, currentIsUsb)
                }
                override fun onPause() {
                    updateMediaSessionPlaybackState(false)
                    nativeSendMediaCommand(10)
                    if (foregroundStarted) startForegroundNotification(currentHostAddr, currentIsUsb)
                }
                override fun onSkipToNext() { nativeSendMediaCommand(11) }
                override fun onSkipToPrevious() { nativeSendMediaCommand(12) }
                override fun onMediaButtonEvent(mediaButtonIntent: Intent): Boolean {
                    @Suppress("DEPRECATION")
                    val event: KeyEvent? = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
                        mediaButtonIntent.getParcelableExtra(Intent.EXTRA_KEY_EVENT, KeyEvent::class.java)
                    } else {
                        mediaButtonIntent.getParcelableExtra(Intent.EXTRA_KEY_EVENT)
                    }
                    if (event?.action == KeyEvent.ACTION_DOWN) {
                        when (event.keyCode) {
                            KeyEvent.KEYCODE_MEDIA_PLAY,
                            KeyEvent.KEYCODE_MEDIA_PAUSE,
                            KeyEvent.KEYCODE_MEDIA_PLAY_PAUSE -> {
                                updateMediaSessionPlaybackState(!remoteMediaPlaying)
                                nativeSendMediaCommand(10)
                                if (foregroundStarted) startForegroundNotification(currentHostAddr, currentIsUsb)
                                return true
                            }
                            KeyEvent.KEYCODE_MEDIA_NEXT -> {
                                nativeSendMediaCommand(11)
                                return true
                            }
                            KeyEvent.KEYCODE_MEDIA_PREVIOUS -> {
                                nativeSendMediaCommand(12)
                                return true
                            }
                        }
                    }
                    return super.onMediaButtonEvent(mediaButtonIntent)
                }
            })
            isActive = true
        }
        updateMediaSessionPlaybackState(true)
    }

    /// Report the active Bluetooth route independently of the Compose UI.
    ///
    /// `MainScreen` polls the same route while it is visible, but playback can
    /// be started from the Quick Settings tile, the screen can be off, or
    /// another app can be in front. `AudioDeviceCallback` fires on every route
    /// add/remove, so the A2DP allowance stays correct without the UI.
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
        val (fallbackBtDelayMs, routeMode) = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            val attrs = AudioAttributes.Builder()
                .setUsage(AudioAttributes.USAGE_MEDIA)
                .setContentType(AudioAttributes.CONTENT_TYPE_MUSIC)
                .build()
            val routedType = am.getAudioDevicesForAttributes(attrs).firstOrNull()?.type
            when (routedType) {
                AudioDeviceInfo.TYPE_BLUETOOTH_A2DP -> 145 to "A2DP"
                AudioDeviceInfo.TYPE_BLUETOOTH_SCO -> 60 to "SCO"
                AudioDeviceInfo.TYPE_BLE_HEADSET,
                AudioDeviceInfo.TYPE_BLE_SPEAKER -> 45 to "BLE-Audio"
                AudioDeviceInfo.TYPE_HEARING_AID -> 65 to "Hearing-Aid"
                else -> if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU &&
                    routedType == AudioDeviceInfo.TYPE_BLE_BROADCAST
                ) {
                    45 to "BLE-Audio"
                } else {
                    0 to "None"
                }
            }
        } else {
            @Suppress("DEPRECATION")
            val a2dpRouted = am.isBluetoothA2dpOn
            @Suppress("DEPRECATION")
            val scoRouted = am.isBluetoothScoOn()
            when {
                a2dpRouted -> 145 to "A2DP"
                scoRouted -> 60 to "SCO"
                else -> 0 to "None"
            }
        }
        val reportedDelayMs = if (fallbackBtDelayMs > 0) {
            val prefs = getSharedPreferences("audioshare_ultra_prefs", Context.MODE_PRIVATE)
            val manualOverride = prefs.getInt("bt_delay_override_ms", 0)
            if (manualOverride in 40..450) {
                // Live BT Lip-Sync Calibration slider value wins over the
                // estimate for any active Bluetooth route (A2DP, BLE Audio, SCO).
                manualOverride
            } else if (fallbackBtDelayMs == 145) {
                // Android 10-14 blocks reflection on hidden getOutputLatency;
                // real SBC/AAC/LDAC earbuds typically sit around 185-240 ms.
                val avdtpMs = runCatching {
                    val m = am.javaClass.getMethod("getOutputLatency", Int::class.javaPrimitiveType)
                    (m.invoke(am, AudioManager.STREAM_MUSIC) as? Int) ?: 0
                }.getOrDefault(0)
                if (avdtpMs in 35..450) avdtpMs else 185
            } else {
                fallbackBtDelayMs
            }
        } else {
            0
        }
        if (reportedDelayMs != lastReportedBtDelayMs || routeMode != lastReportedBtRouteMode) {
            lastReportedBtDelayMs = reportedDelayMs
            lastReportedBtRouteMode = routeMode
            runCatching {
                nativeLogEvent(
                    0,
                    "bt-route",
                    "Bluetooth route delay changed to ${reportedDelayMs}ms (mode=${routeMode})",
                )
            }
        }
        nativeReportBluetoothDelayMs(reportedDelayMs)
    }

    private fun requestAudioFocus(@Suppress("UNUSED_PARAMETER") useDolby: Boolean) {
        val am = getSystemService(Context.AUDIO_SERVICE) as AudioManager
        // Reconnects call onStartCommand again; abandon the previous request
        // instead of overwriting it and leaking focus ownership.
        focusRequest?.let { audioManager?.abandonAudioFocusRequest(it) }
        audioManager = am
        // Always request MEDIA/MUSIC focus to match the AAudio USAGE_MEDIA /
        // CONTENT_TYPE_MUSIC stream attributes. Using GAME/SONIFICATION caused
        // OEM audio HALs (ColorOS/MIUI/OneUI) to apply quieter UI/game volume
        // curves (~6 dB lower than music apps at 100% media volume). Raw mode
        // still bypasses OEM effects via AAudio EXCLUSIVE + SPATIALIZATION_NEVER.
        val attributes = AudioAttributes.Builder()
            .setUsage(AudioAttributes.USAGE_MEDIA)
            .setContentType(AudioAttributes.CONTENT_TYPE_MUSIC)
            .build()
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

        // Android 12+ ADPF (Dynamic Performance Framework): Request a 2.5 ms
        // real-time scheduler boost for the native audio threads, not for the
        // service main thread that merely calls JNI.
        refreshAdpfHintSession()
    }

    private fun refreshAdpfHintSession() {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.S) return
        if (!isStreaming && !isPartyCasting && !micCaptureActive) return
        try {
            perfHintSession?.close()
        } catch (_: Exception) {
        }
        perfHintSession = null
        try {
            val hintMgr = getSystemService(PerformanceHintManager::class.java)
            val nativeTids = runCatching { nativeGetRealtimeTids() }
                .getOrDefault(IntArray(0))
                .distinct()
                .toIntArray()
            if (nativeTids.isEmpty()) return
            perfHintSession = hintMgr?.createHintSession(nativeTids, 2_500_000L)
        } catch (_: Exception) {
        }
    }

    private fun scheduleAdpfRefresh() {
        adpfHandler.removeCallbacks(adpfRefreshRunnable)
        adpfHandler.postDelayed(adpfRefreshRunnable, 500L)
    }

    private fun isUsbHost(hostAddr: String): Boolean {
        if (hostAddr.startsWith("127.0.0.1") ||
            hostAddr.startsWith("[::1]") ||
            hostAddr.startsWith("192.168.42.")
        ) {
            return true
        }
        // Bracket-aware: "[fd00::1]:65530" must yield "fd00::1", not "".
        val hostIp = if (hostAddr.startsWith("[")) {
            hostAddr.substringAfter("[").substringBefore("]")
        } else {
            hostAddr.substringBefore(':')
        }
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

    /**
     * Redeem the one-shot MediaProjection consent in the service, after the
     * MEDIA_PROJECTION foreground-service type is active. Android 14+ throws
     * `SecurityException: Media projections require a foreground service of
     * type ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PROJECTION` if
     * getMediaProjection() runs first.
     */
    private fun startPartyCast(intent: Intent): Int {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.Q) {
            broadcastPartyCastState(false)
            if (isStreaming) return START_STICKY
            ensureForegroundBeforeStop()
            stopStreamAndService()
            return START_NOT_STICKY
        }

        val resultCode = intent.getIntExtra(
            "PROJECTION_RESULT_CODE",
            Activity.RESULT_CANCELED
        )
        @Suppress("DEPRECATION")
        val resultData: Intent? = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            intent.getParcelableExtra("PROJECTION_DATA", Intent::class.java)
        } else {
            intent.getParcelableExtra("PROJECTION_DATA")
        }
        if (resultCode != Activity.RESULT_OK || resultData == null) {
            broadcastPartyCastState(false)
            if (isStreaming) return START_STICKY
            ensureForegroundBeforeStop()
            stopStreamAndService()
            return START_NOT_STICKY
        }

        // A repeated toggle can leave the previous caster alive. Drop it
        // before installing the new projection.
        val previous = activeCaster
        activeCaster = null
        projectionActive = false
        previous?.stopCasting()

        // Empty is a valid Party target list: the transmitter advertises a
        // beacon and peer phones register themselves by HELLO/feedback. The
        // old 255.255.255.255 fallback was filtered out by the native parser
        // anyway (broadcast destinations burn Wi-Fi airtime), so it only hid
        // the discovery-only case.
        val rawTargets = intent.getStringExtra("TARGET_HOSTS").orEmpty()
        val targets = rawTargets
            .split(',', ';', ' ')
            .map { it.trim() }
            .filter { it.isNotEmpty() }

        if (!isStreaming) {
            currentHostAddr = "Party Audio Broadcast"
            currentIsUsb = false
        }

        // Playback capture is not microphone capture. A previous video-call
        // session may still hold the native hardware-mic transmitter; release
        // it here so the service stays the single owner of phone-mic state.
        val stopGen = micGen.incrementAndGet()
        // The generation bump above cancels a START still queued on the single
        // engine thread (ACTION_START_PHONE_MIC gen guard). This bounded barrier
        // additionally waits for a START already executing nativeStartPhoneMic,
        // so the stop cannot be overtaken before caster.startCasting() below.
        val stoppedOnExecutor = runCatching {
            engineExecutor.submit {
                if (stopGen == micGen.get()) {
                    runCatching { nativeStopPhoneMic() }
                }
            }.get(1500, java.util.concurrent.TimeUnit.MILLISECONDS)
            true
        }.getOrDefault(false)
        // Timeout / rejection fallback: must NOT stop a mic session that began
        // while this thread was blocked — hence the generation re-check.
        if (!stoppedOnExecutor && stopGen == micGen.get()) {
            runCatching { nativeStopPhoneMic() }
        }
        broadcastPhoneMicState(false)
        micCaptureActive = false
        projectionActive = true
        var projection: MediaProjection? = null
        try {
            // This must complete BEFORE getMediaProjection(). The consent
            // data is already in hand, so the system can validate the type.
            startForegroundNotification(
                currentHostAddr,
                currentIsUsb,
                includeProjectionType = true
            )
            acquireRealtimeHardwareLocks()

            val mgr = getSystemService(Context.MEDIA_PROJECTION_SERVICE) as MediaProjectionManager
            val acquired = mgr.getMediaProjection(resultCode, resultData)
                ?: return failPartyCastStart("MediaProjection permission was revoked or unavailable")
            projection = acquired
            val prefs = getSharedPreferences("audioshare_ultra_prefs", Context.MODE_PRIVATE)
            val pin = (intent.getStringExtra("PIN") ?: prefs.getString("pin_code", "") ?: "").trim()
            activePartyPin = pin
            // Apply the user's locked Party codec & bitrate before starting the
            // transmitter so Party Mode never auto-switches mid-stream.
            val partyCodecProfile = prefs.getInt("party_codec_profile", 0).let {
                if (it in 0..2) it else 0
            }
            val partyBitrateKbps = prefs.getInt("party_bitrate_kbps", 256).coerceIn(96, 320)
            runCatching { nativeSetPartyCodecConfig(partyCodecProfile, partyBitrateKbps) }

            var casterRef: PhoneAudioCaster? = null
            val caster = PhoneAudioCaster(acquired, targets, pin) {
                // Ignore a late callback from a caster that was already
                // replaced or explicitly stopped.
                val stopped = casterRef
                if (stopped != null && activeCaster === stopped) {
                    activeCaster = null
                    handlePartyCastStopped()
                }
            }
            casterRef = caster
            if (!caster.startCasting()) {
                return failPartyCastStart("Could not start internal audio capture (check Android AudioRecord/mic availability)")
            }
            activeCaster = caster
            isPartyCastMuted = false
            runCatching { nativeSetPartyCastMuted(false) }
            // Default ON: mute this phone's speaker to avoid the ~60 ms
            // same-room slapback echo; peers/PCs become the synced speakers.
            if (prefs.getBoolean("party_mute_local", true)) {
                applyLocalPartySpeakerMute(true)
            }
            broadcastPartyCastState(true)
            runCatching {
                nativeLogEvent(0, "party-cast", "Party cast started (${targets.size} target(s))")
            }
            // Re-publish the notification now isPartyCasting is true so the
            // Party Host actions (Mute Cast / Stop Party) replace Pause PC.
            startForegroundNotification(
                currentHostAddr,
                currentIsUsb,
                includeProjectionType = true
            )
            partyMediaHandler.removeCallbacks(partyMediaPollRunnable)
            partyMediaHandler.postDelayed(partyMediaPollRunnable, 250L)
            refreshAdpfHintSession()
            scheduleAdpfRefresh()
            // A projection token cannot be resumed after process death, so a
            // party-cast-only service must not be sticky. A concurrent PC
            // stream keeps its normal sticky restart behavior.
            return if (isStreaming) START_STICKY else START_NOT_STICKY
        } catch (e: Exception) {
            runCatching { projection?.stop() }
            Log.e("AudioEngineService", "Unable to start party cast", e)
            ensureForegroundBeforeStop()
            return failPartyCastStart("Party Mode failed: ${e.message ?: "unexpected error"}")
        }
    }

    private fun failPartyCastStart(reason: String = "Unable to start Party Mode"): Int {
        runCatching {
            nativeLogEvent(2, "party-cast", "Party cast start failed: $reason")
        }
        val caster = activeCaster
        activeCaster = null
        caster?.stopCasting()
        projectionActive = false
        activePartyPin = ""
        isPartyCastMuted = false
        runCatching { nativeSetPartyCastMuted(false) }
        partyMediaHandler.removeCallbacks(partyMediaPollRunnable)
        applyLocalPartySpeakerMute(false)
        micCaptureActive = false
        val gen = micGen.incrementAndGet()
        runOnEngine {
            if (gen == micGen.get()) nativeStopPhoneMic()
        }
        if (isStreaming) {
            broadcastPartyCastState(false, reason)
            runCatching {
                startForegroundNotification(
                    currentHostAddr,
                    currentIsUsb,
                    includeProjectionType = false
                )
            }
            return START_STICKY
        }
        stopStreamAndService()
        broadcastPartyCastState(false, reason)
        return START_NOT_STICKY
    }

    private fun stopPartyCast(): Int {
        runCatching {
            nativeLogEvent(0, "party-cast", "Party cast stopped (user request)")
        }
        val caster = activeCaster
        activeCaster = null
        projectionActive = false
        activePartyPin = ""
        isPartyCastMuted = false
        runCatching { nativeSetPartyCastMuted(false) }
        partyMediaHandler.removeCallbacks(partyMediaPollRunnable)
        applyLocalPartySpeakerMute(false)
        caster?.stopCasting()
        val gen = micGen.incrementAndGet()
        runOnEngine {
            if (gen == micGen.get()) nativeStopPhoneMic()
        }
        broadcastPartyCastState(false)
        if (isStreaming) {
            runCatching {
                startForegroundNotification(
                    currentHostAddr,
                    currentIsUsb,
                    includeProjectionType = false
                )
            }
            return START_STICKY
        }
        stopStreamAndService()
        return START_NOT_STICKY
    }

    private fun handlePartyCastStopped() {
        runCatching {
            nativeLogEvent(1, "party-cast", "Party cast ended (projection revoked or caster stopped)")
        }
        projectionActive = false
        activePartyPin = ""
        isPartyCastMuted = false
        runCatching { nativeSetPartyCastMuted(false) }
        partyMediaHandler.removeCallbacks(partyMediaPollRunnable)
        applyLocalPartySpeakerMute(false)
        val gen = micGen.incrementAndGet()
        runOnEngine {
            if (gen == micGen.get()) nativeStopPhoneMic()
        }
        broadcastPartyCastState(false)
        if (isStreaming && foregroundStarted) {
            runCatching {
                startForegroundNotification(
                    currentHostAddr,
                    currentIsUsb,
                    includeProjectionType = false
                )
            }
        } else {
            stopStreamAndService()
        }
    }

    /**
     * Service-owned local speaker mute. The saved volume lives here (not in
     * Compose state) so volume is restored on every teardown path, including
     * screen-off notification stops and system projection revocation.
     */
    private fun applyLocalPartySpeakerMute(mute: Boolean) {
        runCatching {
            val am = getSystemService(Context.AUDIO_SERVICE) as AudioManager
            if (mute) {
                val cur = am.getStreamVolume(AudioManager.STREAM_MUSIC)
                if (cur > 0) savedPartyMusicVolume = cur
                am.setStreamVolume(AudioManager.STREAM_MUSIC, 0, 0)
            } else if (savedPartyMusicVolume > 0) {
                // Only restore if the speaker is still muted by us. The user or
                // another app may have raised music volume while we held it at 0.
                if (am.getStreamVolume(AudioManager.STREAM_MUSIC) == 0) {
                    am.setStreamVolume(AudioManager.STREAM_MUSIC, savedPartyMusicVolume, 0)
                }
                savedPartyMusicVolume = -1
            }
        }
    }

    private fun broadcastPartyCastState(active: Boolean, error: String? = null) {
        isPartyCasting = active
        val intent = Intent(ACTION_PARTY_CAST_STATE).apply {
            setPackage(packageName)
            putExtra(EXTRA_PARTY_CAST_ACTIVE, active)
            if (!error.isNullOrBlank()) putExtra(EXTRA_PARTY_CAST_ERROR, error)
        }
        sendBroadcast(intent)
    }

    private fun broadcastPhoneMicState(active: Boolean, error: String? = null) {
        isPhoneMicActive = active
        val intent = Intent(ACTION_PHONE_MIC_STATE).apply {
            setPackage(packageName)
            putExtra(EXTRA_PHONE_MIC_ACTIVE, active)
            if (!error.isNullOrBlank()) putExtra(EXTRA_PHONE_MIC_ERROR, error)
        }
        sendBroadcast(intent)
    }

    private fun broadcastSleepTimer(minutes: Int) {
        sleepRemainingMinutes = minutes
        val intent = Intent(ACTION_SLEEP_TIMER_STATE).apply {
            setPackage(packageName)
            putExtra(EXTRA_SLEEP_REMAINING_MINS, minutes)
        }
        sendBroadcast(intent)
    }

    private fun broadcastStreamState(active: Boolean, error: String?) {
        val intent = Intent(ACTION_STREAM_STATE).apply {
            setPackage(packageName)
            putExtra(EXTRA_STREAM_ACTIVE, active)
            if (error != null) putExtra(EXTRA_STREAM_ERROR, error)
        }
        sendBroadcast(intent)
        // The tile only refreshes while it is listening, so nudge it whenever
        // the stream state changes (including the async connect result).
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.N) {
            runCatching {
                android.service.quicksettings.TileService.requestListeningState(
                    this,
                    android.content.ComponentName(this, QuickConnectTileService::class.java)
                )
            }
        }
    }

    private fun startForegroundNotification(
        hostAddr: String,
        isUsb: Boolean,
        includeMicType: Boolean = micCaptureActive,
        includeProjectionType: Boolean = projectionActive
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

        val openAppIntent = Intent(this, MainActivity::class.java).apply {
            flags = Intent.FLAG_ACTIVITY_SINGLE_TOP or Intent.FLAG_ACTIVITY_CLEAR_TOP
        }
        val openAppPending = PendingIntent.getActivity(
            this, 3, openAppIntent,
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE
        )
        val playPauseIntent = Intent(this, AudioEngineService::class.java).apply {
            action = "ACTION_REMOTE_PLAY_PAUSE"
        }
        val playPausePending = PendingIntent.getService(
            this, 2, playPauseIntent,
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE
        )
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

        val playPauseIcon = if (remoteMediaPlaying) {
            android.R.drawable.ic_media_pause
        } else {
            android.R.drawable.ic_media_play
        }
        val builder = NotificationCompat.Builder(this, channelId)
            .setSmallIcon(R.drawable.ic_qs_tile_audio)
            .setContentIntent(openAppPending)
            .setOngoing(true)
            .setOnlyAlertOnce(true)

        if (isPartyCasting && !isStreaming) {
            // Pause PC / gain Mute are no-ops for a pure Party Host. Expose
            // actions that actually control the phone-hosted broadcast.
            val partyMuteIntent = Intent(this, AudioEngineService::class.java).apply {
                action = ACTION_TOGGLE_PARTY_MUTE
            }
            val partyMutePending = PendingIntent.getService(
                this, 4, partyMuteIntent,
                PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE
            )
            val stopPartyIntent = Intent(this, AudioEngineService::class.java).apply {
                action = ACTION_STOP_PARTY_CAST
            }
            val stopPartyPending = PendingIntent.getService(
                this, 5, stopPartyIntent,
                PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE
            )
            builder
                .setContentTitle("AudioShare Ultra • Party Broadcast")
                .setContentText(
                    if (isPartyCastMuted) "CAST MUTED • Peers on standby"
                    else "ON AIR • Synced multi-device stream"
                )
                .addAction(
                    android.R.drawable.ic_lock_silent_mode,
                    if (isPartyCastMuted) "Unmute Cast" else "Mute Cast",
                    partyMutePending
                )
                .addAction(
                    android.R.drawable.ic_menu_close_clear_cancel,
                    "Stop Party",
                    stopPartyPending
                )
        } else if (micCaptureActive && !isStreaming) {
            val muteMicIntent = Intent(this, AudioEngineService::class.java).apply {
                action = ACTION_TOGGLE_MIC_MUTE
            }
            val muteMicPending = PendingIntent.getService(
                this, 7, muteMicIntent,
                PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE
            )
            val stopMicIntent = Intent(this, AudioEngineService::class.java).apply {
                action = ACTION_STOP_PHONE_MIC
            }
            val stopMicPending = PendingIntent.getService(
                this, 6, stopMicIntent,
                PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE
            )
            builder
                .setContentTitle("AudioShare Ultra • Wireless Mic")
                .setContentText(
                    if (isMicMuted) "MIC MUTED • $hostAddr (Port 65532)"
                    else "MIC LIVE -> $hostAddr (Port 65532)"
                )
                .addAction(
                    android.R.drawable.ic_lock_silent_mode,
                    if (isMicMuted) "Unmute Mic" else "Mute Mic",
                    muteMicPending
                )
                .addAction(
                    android.R.drawable.ic_menu_close_clear_cancel,
                    "Stop Mic",
                    stopMicPending
                )
        } else {
            val playPauseLabel = if (remoteMediaPlaying) "Pause PC" else "Play PC"
            val badge = if (isUsb) "USB Lossless (<11ms)" else "Wi-Fi Lossless + FEC"
            val statusText = if (isLocallyMuted) "MUTED • $hostAddr" else "Streaming • $hostAddr"
            val micSuffix = if (micCaptureActive) {
                if (isMicMuted) " • MIC MUTED" else " • MIC LIVE"
            } else {
                ""
            }
            builder
                .setContentTitle("AudioShare Ultra • $badge")
                .setContentText(statusText + micSuffix)
                .addAction(playPauseIcon, playPauseLabel, playPausePending)
                .addAction(
                    android.R.drawable.ic_lock_silent_mode,
                    if (isLocallyMuted) "Unmute" else "Mute",
                    mutePending
                )
                .addAction(android.R.drawable.ic_menu_close_clear_cancel, "Disconnect", stopPending)
        }
        val notification = builder.build()

        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            // Declare MICROPHONE only while a mic capture actually runs and
            // RECORD_AUDIO is granted. Internal-audio playback capture is a
            // mediaProjection service, not a microphone one.
            val micGranted = checkSelfPermission(android.Manifest.permission.RECORD_AUDIO) ==
                android.content.pm.PackageManager.PERMISSION_GRANTED
            var type = ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PLAYBACK
            if (includeMicType && micGranted) {
                type = type or ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE
            }
            if (includeProjectionType) {
                type = type or ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PROJECTION
            }
            startForeground(101, notification, type)
        } else if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            var type = ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PLAYBACK
            if (includeProjectionType) {
                type = type or ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PROJECTION
            }
            startForeground(101, notification, type)
        } else {
            startForeground(101, notification)
        }
        foregroundStarted = true
    }

    override fun onAudioFocusChange(focusChange: Int) {
        val focusLabel = when (focusChange) {
            AudioManager.AUDIOFOCUS_GAIN -> "GAIN"
            AudioManager.AUDIOFOCUS_LOSS -> "LOSS"
            AudioManager.AUDIOFOCUS_LOSS_TRANSIENT -> "LOSS_TRANSIENT"
            AudioManager.AUDIOFOCUS_LOSS_TRANSIENT_CAN_DUCK -> "LOSS_TRANSIENT_CAN_DUCK"
            else -> "OTHER($focusChange)"
        }
        runCatching {
            nativeLogEvent(
                if (focusChange == AudioManager.AUDIOFOCUS_GAIN) 0 else 1,
                "audio-focus",
                "Audio focus change: $focusLabel"
            )
        }
        if (focusChange == AudioManager.AUDIOFOCUS_LOSS_TRANSIENT ||
            focusChange == AudioManager.AUDIOFOCUS_LOSS ||
            focusChange == AudioManager.AUDIOFOCUS_LOSS_TRANSIENT_CAN_DUCK
        ) {
            lostAudioFocus = true
        }
        if (isLocallyMuted) {
            // Never unmute a user-muted or headphone-unplugged stream when a
            // transient notification finishes and returns AUDIOFOCUS_GAIN.
            return
        }
        val prefs = getSharedPreferences("audioshare_ultra_prefs", Context.MODE_PRIVATE)
        val liveGainDb = prefs.getFloat("gain_db", currentGainDb)
        currentGainDb = liveGainDb
        when (focusChange) {
            AudioManager.AUDIOFOCUS_LOSS_TRANSIENT -> nativeSetGainDb(-96.0f) // Auto-mute during phone call
            AudioManager.AUDIOFOCUS_LOSS -> {
                // Permanent loss outlives the focus transition: keep the mute
                // sticky so a later AUDIOFOCUS_GAIN cannot silently unmute us.
                isLocallyMuted = true
                nativeSetGainDb(-96.0f)
                if (foregroundStarted) {
                    startForegroundNotification(currentHostAddr, currentIsUsb)
                }
            }
            AudioManager.AUDIOFOCUS_LOSS_TRANSIENT_CAN_DUCK ->
                nativeSetGainDb((liveGainDb - 12.0f).coerceAtLeast(-96.0f))
            AudioManager.AUDIOFOCUS_GAIN -> {
                nativeSetGainDb(liveGainDb)
                if (lostAudioFocus && isStreaming && !dolbyEffects) {
                    lostAudioFocus = false
                    runCatching { nativeRequestExclusiveMmapReclaim() }
                }
            }
        }
    }

    /** Run a blocking engine call off the main thread, tolerating shutdown. */
    private fun runOnEngine(block: () -> Unit) {
        runCatching {
            engineExecutor.execute {
                try {
                    block()
                } catch (_: Exception) {
                }
            }
        }
    }

    /**
     * Stopping an FGS that never reached startForeground() violates the
     * foreground-service contract, so publish a minimal notification first.
     */
    private fun ensureForegroundBeforeStop() {
        if (!foregroundStarted) {
            startForegroundNotification(
                currentHostAddr.ifBlank { "Stopping…" },
                currentIsUsb,
                includeMicType = false,
                includeProjectionType = false
            )
        }
    }

    /**
     * Release OS resources that belong exclusively to PC->Phone audio playback
     * (AudioFocus, BT route callback, becoming-noisy receiver, and MediaSession).
     * Called on both full teardown and `ACTION_STOP_CLIENT_ONLY` so an active
     * Party Mode / Phone Mic session never holds a dead MediaSession that
     * hijacks peer media keys.
     */
    private fun releaseClientPlaybackResources() {
        focusRequest?.let { audioManager?.abandonAudioFocusRequest(it) }
        focusRequest = null
        lostAudioFocus = false
        audioDeviceCallback?.let { callback ->
            try {
                val am = getSystemService(Context.AUDIO_SERVICE) as AudioManager
                am.unregisterAudioDeviceCallback(callback)
            } catch (_: Exception) {
            }
        }
        audioDeviceCallback = null
        audioManager = null
        if (noisyReceiverRegistered) {
            try {
                unregisterReceiver(becomingNoisyReceiver)
            } catch (_: Exception) {}
            noisyReceiverRegistered = false
        }
        mediaSession?.isActive = false
        mediaSession?.release()
        mediaSession = null
    }

    private fun stopStreamAndService() {
        if (serviceStopped) return
        serviceStopped = true
        // Invalidate any in-flight native-start postback before teardown.
        val cGen = connectGen.incrementAndGet()
        val mGen = micGen.incrementAndGet()
        // Drop the caster before stopping the foreground service so a late
        // projection callback cannot re-enter teardown.
        val caster = activeCaster
        activeCaster = null
        projectionActive = false
        activePartyPin = ""
        isPartyCastMuted = false
        isMicMuted = false
        isLocallyMuted = false
        runCatching { nativeSetPartyCastMuted(false) }
        partyMediaHandler.removeCallbacks(partyMediaPollRunnable)
        applyLocalPartySpeakerMute(false)
        caster?.stopCasting()
        releaseClientPlaybackResources()
        // Process-wide single-thread executor + generation guard ensures a
        // rapid Service destroy/recreate can NEVER let Instance #1's async
        // teardown kill Instance #2's freshly started Rust engine.
        runOnEngine {
            if (mGen == micGen.get()) nativeStopPhoneMic()
            if (cGen == connectGen.get()) nativeStopClient()
        }
        isStreaming = false
        sleepHandler.removeCallbacks(sleepTickRunnable)
        stopSilenceWatchdog()
        broadcastSleepTimer(0)
        broadcastStreamState(false, null)
        micCaptureActive = false
        broadcastPartyCastState(false)
        broadcastPhoneMicState(false)
        foregroundStarted = false
        adpfHandler.removeCallbacks(adpfRefreshRunnable)
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
        serviceDestroyed = true
        stopStreamAndService()
        super.onDestroy()
    }

    override fun onBind(intent: Intent?): IBinder? = null
}
