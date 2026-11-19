package com.audioshare.ultra.ui

import android.app.Application
import android.content.Context
import androidx.lifecycle.AndroidViewModel
import com.audioshare.ultra.service.AudioEngineService
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.update

data class DiscoveredHostItem(
    val name: String,
    val addr: String,
    val isUsb: Boolean,
    val encrypted: Boolean
)

data class AudioShareUiState(
    val selectedTab: Int = 0,
    val hostAddress: String = "192.168.42.129:65530",
    val pinCode: String = "",
    val isConnected: Boolean = false,
    val sharePhoneMic: Boolean = false,
    val castPhoneInternalAudio: Boolean = false,
    val sleepRemainingMins: Int = 0,
    val preampGainDb: Float = 0f,
    val bassDb: Float = 0f,
    val vocalDb: Float = 0f,
    val trebleDb: Float = 0f,
    val nightMode: Boolean = false,
    val crossfeed: Boolean = false,
    val gamingLock: Boolean = false,
    val dolbyEffects: Boolean = false,
    val latencyDebug: Boolean = false,
    val channelRole: Int = 0,
    val qualityMode: Int = 0,
    val compressedBitrateKbps: Int = 256,
    val useOpus: Boolean = true,
    val btDelayOverrideMs: Int = 0,
    val forwardPartyToPc: Boolean = false,
    val muteHostSpeakerInParty: Boolean = false,
    val showManualConnect: Boolean = false,
    val showTelemetryDrawer: Boolean = false,
    val showEqDrawer: Boolean = false,
    val showCodecDrawer: Boolean = false,
    val discoveredHosts: List<DiscoveredHostItem> = emptyList(),
    val telemetry: LongArray = LongArray(25),
    val vuPeakHold: Float = 0f,
    val btEarbudsDetected: Boolean = false
)

/** Rotation-safe state holder for the Compose UI. */
class AudioShareViewModel(app: Application) : AndroidViewModel(app) {
    private val prefs = app.getSharedPreferences("audioshare_ultra_prefs", Context.MODE_PRIVATE)

    private val _uiState = MutableStateFlow(loadInitial())
    val uiState: StateFlow<AudioShareUiState> = _uiState.asStateFlow()

    private fun loadInitial(): AudioShareUiState {
        val savedBitrate = prefs.getInt("target_bitrate_kbps", 256).let {
            if (it == 96 || it == 160 || it == 256 || it == 320) it else 256
        }
        val hud = prefs.getBoolean("latency_debug", false)
        return AudioShareUiState(
            hostAddress = prefs.getString("last_host_addr", "192.168.42.129:65530") ?: "192.168.42.129:65530",
            pinCode = prefs.getString("pin_code", "") ?: "",
            isConnected = AudioEngineService.isStreaming,
            sharePhoneMic = AudioEngineService.isPhoneMicActive,
            castPhoneInternalAudio = AudioEngineService.isPartyCasting,
            sleepRemainingMins = AudioEngineService.sleepRemainingMinutes,
            preampGainDb = prefs.getFloat("gain_db", 0f),
            bassDb = prefs.getFloat("bass_db", 0f),
            vocalDb = prefs.getFloat("vocal_db", 0f),
            trebleDb = prefs.getFloat("treble_db", 0f),
            nightMode = prefs.getBoolean("night_mode", false),
            crossfeed = prefs.getBoolean("crossfeed", false),
            gamingLock = prefs.getBoolean("gaming_lock", false),
            dolbyEffects = prefs.getBoolean("dolby_fx", false),
            latencyDebug = hud,
            showTelemetryDrawer = hud,
            channelRole = prefs.getInt("channel_role", 0),
            qualityMode = prefs.getInt("quality_mode", 0),
            compressedBitrateKbps = savedBitrate,
            useOpus = prefs.getBoolean("use_opus", true),
            btDelayOverrideMs = prefs.getInt("bt_delay_override_ms", 0),
            forwardPartyToPc = prefs.getBoolean("party_forward_pc", false),
            muteHostSpeakerInParty = prefs.getBoolean("party_mute_local", false)
        )
    }

    fun update(block: (AudioShareUiState) -> AudioShareUiState) {
        _uiState.update(block)
    }

    /**
     * JNI CSV rows are `name|addr|USB|enc` — the 4th field is the beacon's
     * `enc=0/1` flag (absent on older hosts, which are treated as open).
     */
    fun parseDiscoveredCsv(csv: String): List<DiscoveredHostItem> =
        csv.lines().filter { it.isNotBlank() }.mapNotNull { line ->
            val parts = line.split("|")
            val name = parts.getOrNull(0) ?: return@mapNotNull null
            val addr = parts.getOrNull(1) ?: return@mapNotNull null
            val isUsb = parts.getOrNull(2).equals("USB", ignoreCase = true)
            val enc = parts.getOrNull(3) == "1"
            DiscoveredHostItem(name, addr, isUsb, enc)
        }

    /** Telemetry index 24 carries the client's post-volume VU peak (0..100). */
    fun updateTelemetry(raw: LongArray) {
        val vu = raw.getOrElse(24) { 0L }.toInt().coerceIn(0, 100)
        _uiState.update { st ->
            val nextHold = if (vu >= st.vuPeakHold) {
                vu.toFloat()
            } else {
                (st.vuPeakHold - 3.0f).coerceAtLeast(0f)
            }
            st.copy(
                telemetry = raw,
                vuPeakHold = nextHold,
                btEarbudsDetected = raw.getOrElse(17) { 0L } > 0L
            )
        }
    }
}
