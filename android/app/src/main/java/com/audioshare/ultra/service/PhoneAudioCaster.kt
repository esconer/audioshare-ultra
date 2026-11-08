package com.audioshare.ultra.service

import android.annotation.SuppressLint
import android.media.AudioAttributes
import android.media.AudioFormat
import android.media.AudioPlaybackCaptureConfiguration
import android.media.AudioRecord
import android.media.projection.MediaProjection
import android.os.Build
import androidx.annotation.RequiresApi
import kotlin.concurrent.thread

/**
 * Captures internal Android system audio (Media, Games, Unknown) on Android 10+ (API 29+)
 * and broadcasts it via our Rust Lossless + FEC engine to PC headphones or other Android phones!
 */
@RequiresApi(Build.VERSION_CODES.Q)
class PhoneAudioCaster(
    private val mediaProjection: MediaProjection,
    private val targetEndpoints: List<String> // Can be 1 PC or multiple Phones (Party Mode!)
) {
    @Volatile
    private var running = false
    private var audioRecord: AudioRecord? = null

    @SuppressLint("MissingPermission")
    fun startCasting() {
        if (running) return
        running = true

        val captureConfig = AudioPlaybackCaptureConfiguration.Builder(mediaProjection)
            .addMatchingUsage(AudioAttributes.USAGE_MEDIA)
            .addMatchingUsage(AudioAttributes.USAGE_GAME)
            .addMatchingUsage(AudioAttributes.USAGE_UNKNOWN)
            .build()

        val format = AudioFormat.Builder()
            .setEncoding(AudioFormat.ENCODING_PCM_FLOAT)
            .setSampleRate(48000)
            .setChannelMask(AudioFormat.CHANNEL_IN_STEREO)
            .build()

        // 2.5ms quantum at 48kHz stereo float = 240 floats (960 bytes)
        val minBuf = AudioRecord.getMinBufferSize(
            48000,
            AudioFormat.CHANNEL_IN_STEREO,
            AudioFormat.ENCODING_PCM_FLOAT
        ).coerceAtLeast(3840)

        audioRecord = AudioRecord.Builder()
            .setAudioFormat(format)
            .setBufferSizeInBytes(minBuf)
            .setAudioPlaybackCaptureConfig(captureConfig)
            .build()

        audioRecord?.startRecording()

        // Start our Rust lossless transmitter for each target phone / PC
        targetEndpoints.firstOrNull()?.let { primaryTarget ->
            AudioEngineService.nativeStartPhoneMic(primaryTarget)
        }

        thread(name = "as-ultra-phone-caster", isDaemon = true) {
            android.os.Process.setThreadPriority(android.os.Process.THREAD_PRIORITY_URGENT_AUDIO)
            val subframe = FloatArray(240) // 120 stereo frames = 2.5ms
            while (running) {
                val read = audioRecord?.read(subframe, 0, subframe.size, AudioRecord.READ_BLOCKING) ?: 0
                if (read <= 0) break
                AudioEngineService.nativePushCapturedAudio(subframe)
            }
        }
    }

    fun stopCasting() {
        running = false
        try {
            audioRecord?.stop()
            audioRecord?.release()
        } catch (_: Exception) {
        }
        audioRecord = null
    }
}
