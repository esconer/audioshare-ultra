package com.audioshare.ultra.service

import android.annotation.SuppressLint
import android.media.AudioAttributes
import android.media.AudioFormat
import android.media.AudioPlaybackCaptureConfiguration
import android.media.AudioRecord
import android.media.projection.MediaProjection
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.os.Process
import androidx.annotation.RequiresApi
import kotlin.concurrent.thread

/**
 * Captures internal Android system audio (Media, Games, Unknown) on Android 10+ (API 29+)
 * and broadcasts it via our Rust Lossless + FEC engine to PC headphones or other Android phones!
 *
 * The owning foreground service must call `startForeground()` with
 * `FOREGROUND_SERVICE_TYPE_MEDIA_PROJECTION` before constructing this class;
 * Android 14+ rejects `getMediaProjection()` otherwise.
 */
@RequiresApi(Build.VERSION_CODES.Q)
class PhoneAudioCaster(
    private val mediaProjection: MediaProjection,
    private val targetEndpoints: List<String>, // Can be 1 PC or multiple Phones (Party Mode!)
    private val onProjectionStopped: (() -> Unit)? = null
) {
    @Volatile
    private var running = false

    @Volatile
    private var stopRequested = false

    @Volatile
    private var audioRecord: AudioRecord? = null

    private val projectionCallback = object : MediaProjection.Callback() {
        override fun onStop() {
            // The system (or the user via the status bar) revoked projection.
            // Stop the reader and let the owning service drop the
            // MEDIA_PROJECTION foreground-service type.
            stopRequested = true
            stopCasting()
            notifyProjectionStopped()
        }
    }

    private fun notifyProjectionStopped() {
        val callback = onProjectionStopped ?: return
        if (Looper.myLooper() == Looper.getMainLooper()) {
            callback()
        } else {
            Handler(Looper.getMainLooper()).post(callback)
        }
    }

    @SuppressLint("MissingPermission")
    fun startCasting(): Boolean {
        if (running) return true

        stopRequested = false
        var record: AudioRecord? = null
        var nativeStarted = false
        try {
            // Android 14+ requires a registered callback for every projection
            // session. It also delivers the OS revocation that used to leave
            // this caster pushing a dead AudioRecord.
            mediaProjection.registerCallback(
                projectionCallback,
                Handler(Looper.getMainLooper())
            )

            val captureConfig = AudioPlaybackCaptureConfiguration.Builder(mediaProjection)
                .addMatchingUsage(AudioAttributes.USAGE_MEDIA)
                .addMatchingUsage(AudioAttributes.USAGE_GAME)
                .addMatchingUsage(AudioAttributes.USAGE_UNKNOWN)
                // Never capture AudioShare Ultra's own playback. Without this,
                // a phone that is also listening to the Party stream would
                // feed its own speaker output back into the transmitter.
                .excludeUid(Process.myUid())
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

            val built = AudioRecord.Builder()
                .setAudioFormat(format)
                .setBufferSizeInBytes(minBuf)
                .setAudioPlaybackCaptureConfig(captureConfig)
                .build()
            record = built

            // A single-app or non-capturable source yields an uninitialized
            // AudioRecord; starting it would throw IllegalStateException on
            // the caller's thread.
            if (built.state != AudioRecord.STATE_INITIALIZED) {
                abortStart(built, stopNative = false)
                return false
            }

            built.startRecording()
            if (built.recordingState != AudioRecord.RECORDSTATE_RECORDING) {
                abortStart(built, stopNative = false)
                return false
            }

            // Start our Rust lossless transmitter for all target phones / PCs
            // (Party Mode!) before the reader thread pushes samples.
            val targetListStr = targetEndpoints.joinToString(",")
            nativeStarted = AudioEngineService.nativeStartPhoneMic(targetListStr, false)
            if (!nativeStarted) {
                abortStart(record, stopNative = false)
                return false
            }

            // A revocation can race the setup above. Never start the reader
            // on a projection that is already gone.
            if (stopRequested) {
                abortStart(record, stopNative = true)
                return false
            }

            audioRecord = built
            if (stopRequested) {
                abortStart(built, stopNative = true)
                return false
            }
            running = true
            val activeRecord = built
            thread(name = "as-ultra-phone-caster", isDaemon = true) {
                try {
                    android.os.Process.setThreadPriority(
                        android.os.Process.THREAD_PRIORITY_URGENT_AUDIO
                    )
                    val subframe = FloatArray(240) // 120 stereo frames = 2.5 ms
                    var consecutiveZeroReads = 0
                    while (running) {
                        var offset = 0
                        while (offset < subframe.size && running) {
                            val read = activeRecord.read(
                                subframe,
                                offset,
                                subframe.size - offset,
                                AudioRecord.READ_BLOCKING
                            )
                            if (read < 0) break
                            if (read == 0) {
                                // A transient route switch or a momentarily
                                // empty capture FIFO can return 0 without
                                // ending the projection. Retry instead of
                                // silently killing Party Mode while the UI
                                // switch still says it is casting. Bound the
                                // retry so a permanently dead route still
                                // tears down and notifies the service.
                                consecutiveZeroReads++
                                if (consecutiveZeroReads > 2000) break
                                Thread.sleep(1)
                                continue
                            }
                            consecutiveZeroReads = 0
                            offset += read
                        }
                        // Never push a partially filled subframe: its tail
                        // would repeat samples from the previous 2.5 ms
                        // quantum.
                        if (offset < subframe.size) break
                        AudioEngineService.nativePushCapturedAudio(subframe)
                    }
                } catch (_: Throwable) {
                    // A native read failure must still release the record and
                    // notify the service in `finally`.
                } finally {
                    val wasStopRequested = stopRequested
                    running = false
                    audioRecord = null
                    runCatching { activeRecord.stop() }
                    runCatching { activeRecord.release() }
                    runCatching { mediaProjection.unregisterCallback(projectionCallback) }
                    runCatching { mediaProjection.stop() }
                    if (!wasStopRequested) {
                        // The reader died on its own (device busy, route
                        // change, revoked consent). The service must drop the
                        // projection foreground-service type.
                        notifyProjectionStopped()
                    }
                }
            }
            return true
        } catch (_: Exception) {
            abortStart(record, nativeStarted)
            return false
        }
    }

    /**
     * Releases every resource acquired by a failed start. `release()` is safe
     * here because no reader thread was started yet.
     */
    private fun abortStart(record: AudioRecord?, stopNative: Boolean) {
        if (stopNative) {
            runCatching { AudioEngineService.nativeStopPhoneMic() }
        }
        running = false
        audioRecord = null
        runCatching { record?.stop() }
        runCatching { record?.release() }
        runCatching { mediaProjection.unregisterCallback(projectionCallback) }
        runCatching { mediaProjection.stop() }
    }

    fun stopCasting() {
        if (!running && audioRecord == null) return
        stopRequested = true
        running = false
        // `AudioRecord.stop()` is safe from another thread and unblocks a
        // pending READ_BLOCKING call. Do not release here: the reader thread
        // owns the final teardown so native memory is never freed mid-read.
        runCatching { audioRecord?.stop() }
    }
}
