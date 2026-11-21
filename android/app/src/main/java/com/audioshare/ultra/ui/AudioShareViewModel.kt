package com.audioshare.ultra.ui

import android.app.Application
import android.content.Context
import androidx.compose.runtime.Immutable
import androidx.lifecycle.AndroidViewModel
import com.audioshare.ultra.service.AudioEngineService
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.update

@Immutable
data class DiscoveredHostItem(
    val name: String,
    val addr: String,
    val isUsb: Boolean,
    val encrypted: Boolean
)

/**
 * Immutable, Compose-friendly view of the 25-slot JNI telemetry array.
 *
 * Every slot is a primitive `Long`, so [TelemetrySnapshot] is safe to pass into
 * leaf composables: a 16 Hz telemetry tick only re-executes the leaves that
 * actually read the snapshot, not the whole screen. The index-based accessors
 * mirror the native `stats[]` contract used by the JNI layer and keep the
 * existing UI formulas readable.
 */
@Immutable
data class TelemetrySnapshot(
    val rxPackets: Long = 0L,
    val losslessFrames: Long = 0L,
    val fecHealed: Long = 0L,
    val wsolaPlc: Long = 0L,
    val clockDriftPpm: Long = 0L,
    val authErrors: Long = 0L,
    val hostSilenceSec: Long = 0L,
    val dtxSilent: Long = 0L,
    val hostVolumePct: Long = 0L,
    val groupDelayMs: Long = 0L,
    val dacMs: Long = 0L,
    val packetizationUs: Long = 0L,
    val jitterUs: Long = 0L,
    val adaptiveUs: Long = 0L,
    val targetFifoUs: Long = 0L,
    val bufferedUs: Long = 0L,
    val groupSyncUs: Long = 0L,
    val btMs: Long = 0L,
    val prebufferReady: Long = 0L,
    val codecMode: Long = 0L,
    val estimatedLatencyTenthsMs: Long = 0L,
    val codecLookaheadTenthsMs: Long = 0L,
    val partyPacked: Long = 0L,
    val partySyncMs: Long = 0L,
    val vuPeak: Long = 0L
) {
    operator fun get(index: Int): Long = when (index) {
        0 -> rxPackets
        1 -> losslessFrames
        2 -> fecHealed
        3 -> wsolaPlc
        4 -> clockDriftPpm
        5 -> authErrors
        6 -> hostSilenceSec
        7 -> dtxSilent
        8 -> hostVolumePct
        9 -> groupDelayMs
        10 -> dacMs
        11 -> packetizationUs
        12 -> jitterUs
        13 -> adaptiveUs
        14 -> targetFifoUs
        15 -> bufferedUs
        16 -> groupSyncUs
        17 -> btMs
        18 -> prebufferReady
        19 -> codecMode
        20 -> estimatedLatencyTenthsMs
        21 -> codecLookaheadTenthsMs
        22 -> partyPacked
        23 -> partySyncMs
        24 -> vuPeak
        else -> 0L
    }

    /** Drop-in replacement for the old `LongArray.getOrElse(i) { default }` reads. */
    fun getOrElse(index: Int, defaultValue: () -> Long): Long =
        if (index in 0..24) get(index) else defaultValue()

    companion object {
        fun fromRaw(raw: LongArray): TelemetrySnapshot = TelemetrySnapshot(
            rxPackets = raw.getOrElse(0) { 0L },
            losslessFrames = raw.getOrElse(1) { 0L },
            fecHealed = raw.getOrElse(2) { 0L },
            wsolaPlc = raw.getOrElse(3) { 0L },
            clockDriftPpm = raw.getOrElse(4) { 0L },
            authErrors = raw.getOrElse(5) { 0L },
            hostSilenceSec = raw.getOrElse(6) { 0L },
            dtxSilent = raw.getOrElse(7) { 0L },
            hostVolumePct = raw.getOrElse(8) { 0L },
            groupDelayMs = raw.getOrElse(9) { 0L },
            dacMs = raw.getOrElse(10) { 0L },
            packetizationUs = raw.getOrElse(11) { 0L },
            jitterUs = raw.getOrElse(12) { 0L },
            adaptiveUs = raw.getOrElse(13) { 0L },
            targetFifoUs = raw.getOrElse(14) { 0L },
            bufferedUs = raw.getOrElse(15) { 0L },
            groupSyncUs = raw.getOrElse(16) { 0L },
            btMs = raw.getOrElse(17) { 0L },
            prebufferReady = raw.getOrElse(18) { 0L },
            codecMode = raw.getOrElse(19) { 0L },
            estimatedLatencyTenthsMs = raw.getOrElse(20) { 0L },
            codecLookaheadTenthsMs = raw.getOrElse(21) { 0L },
            partyPacked = raw.getOrElse(22) { 0L },
            partySyncMs = raw.getOrElse(23) { 0L },
            vuPeak = raw.getOrElse(24) { 0L }
        )
    }
}

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
    val muteHostSpeakerInParty: Boolean = true,
    val partyCodecProfile: Int = 0,
    val partyBitrateKbps: Int = 256,
    val showManualConnect: Boolean = false,
    val showTelemetryDrawer: Boolean = false,
    val showEqDrawer: Boolean = false,
    val showCodecDrawer: Boolean = false,
    val discoveredHosts: List<DiscoveredHostItem> = emptyList(),
    val telemetry: TelemetrySnapshot = TelemetrySnapshot(),
    val vuPeakHold: Float = 0f,
    val btEarbudsDetected: Boolean = false,
    // One-shot gesture guards that used to be Compose-local; keeping them in
    // the ViewModel means a permission dialog surviving rotation no longer
    // silently drops the pending action.
    val pendingInternalAudioCast: Boolean = false,
    val partySilenceWarning: Boolean = false
)

/** Rotation-safe state holder for the Compose UI. */
class AudioShareViewModel(app: Application) : AndroidViewModel(app) {
    private val prefs = app.getSharedPreferences("audioshare_ultra_prefs", Context.MODE_PRIVATE)

    private val _uiState = MutableStateFlow(loadInitial())
    val uiState: StateFlow<AudioShareUiState> = _uiState.asStateFlow()

    private fun loadInitial(): AudioShareUiState {
        val savedBitrate = prefs.getInt("target_bitrate_kbps", 256).let {
            if (it == 96 || it == 160 || it == 256 || it == 320 || it == 510) it else 256
        }
        val savedPartyCodec = prefs.getInt("party_codec_profile", 0).coerceIn(0, 3)
        val savedPartyBitrate = prefs.getInt("party_bitrate_kbps", 256).let {
            if (it == 96 || it == 160 || it == 256 || it == 320) it else 256
        }
        val hud = prefs.getBoolean("latency_debug", false)
        val savedHost =
            prefs.getString("last_host_addr", "192.168.42.129:65530") ?: "192.168.42.129:65530"
        return AudioShareUiState(
            hostAddress = savedHost,
            pinCode = HostPinStore.getPin(prefs, savedHost),
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
            muteHostSpeakerInParty = prefs.getBoolean("party_mute_local", true),
            partyCodecProfile = savedPartyCodec,
            partyBitrateKbps = savedPartyBitrate
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
                telemetry = TelemetrySnapshot.fromRaw(raw),
                vuPeakHold = nextHold,
                btEarbudsDetected = raw.getOrElse(17) { 0L } > 0L
            )
        }
    }
}
