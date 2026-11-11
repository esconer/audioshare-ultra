# AudioShare Ultra

A hard real-time, studio-grade wireless and wired audio routing system that turns
an Android phone (or secondary PC) into a zero-configuration, ultra-low-latency
speaker and microphone for Windows and Linux.

Inspired by the original audio-share project, this is an independent
reimplementation focused on deterministic latency, loss resilience, and a shared
Rust core across desktop and Android.

## Highlights

- **Bit-perfect lossless audio** — causal 2nd-order LPC with zero codec lookahead,
  typically 25–35% smaller than raw PCM16.
- **Loss resilience** — `[N-1]`/`[N-2]` forward error correction plus pitch-period
  WSOLA packet-loss concealment with raised-cosine crossfades.
- **Clock-drift immunity** — windowed-minimum DLL and a 4-point Hermite cubic
  fractional resampler; stable over multi-hour sessions.
- **Low jitter** — 64-slot wait-free sequencer and lock-free burst-decoupler ring
  decouple the network quantum from the audio device burst size.
- **Smart DTX** — silence is not transmitted; a −90 dBFS keep-alive keeps
  Bluetooth earbuds awake without draining the battery.
- **Optional Opus backend** — live-switchable pure-Rust Opus for compressed
  profiles, with automatic per-packet detection and VBR fallback.
- **Hitless USB/Wi-Fi failover** — active-active feedback and deduplication.
- **Encrypted streaming** — optional 1-tap PIN pairing with ChaCha20 and MAC32
  authentication.
- **Cross-platform** — native Rust desktop GUI (egui), CLI, Windows/Linux host
  engine, and a Kotlin/Jetpack Compose Android app backed by a Rust JNI core.

## Workspace Layout

```
crates/
  protocol/       Wire format, CRC16, crypto, LPC codec, FEC, DSP, sequencer
  host-engine/    Capture, downmix, limiter, discovery, hotspot, virtual mic
  client-engine/  Receive, jitter buffering, PLC, AAudio output
  desktop-gui/    egui desktop application
  cli/            `as-ultra` command-line host/client/benchmark
  android-jni/    Android JNI bridge, AAudio output, phone-mic transmitter
android/          Self-contained Gradle project (Kotlin + Compose)
.github/          Multi-platform release workflow
```

## Building

Requirements:

- Rust stable (edition 2021)
- **Linux:** ALSA and pkg-config development headers
  (`libasound2-dev` / `alsa-lib-devel`, `pkg-config`)
- **Windows:** no extra system dependencies
- **Android:** Android SDK, NDK, and `cargo-ndk`

```bash
# Desktop GUI
cargo build --release -p audioshare-desktop-gui

# CLI
cargo build --release -p audioshare-cli

# Tests
cargo test --workspace
```

## Usage

```bash
# Start a host (PC speaker audio -> clients)
as-ultra host

# Stream to a host (replace with the discovered host IP)
as-ultra client <host-ip>

# Optional USB/tethering mode
as-ultra host --usb

# Play a test tone instead of capturing the system output
as-ultra host --test-tone

# Opt-in live latency breakdown (host and client HUDs)
as-ultra host --latency-debug
as-ultra client <host-ip> --latency-debug

# Optional Opus backend for compressed quality profiles
as-ultra host --libopus

# Run the built-in packet-loss/latency benchmark
as-ultra benchmark
```

`--latency-debug` (or `--debug`) shows the live packetization, jitter, FIFO,
DAC/Bluetooth, and estimated one-way latency components without changing audio
behavior. It can also be enabled with `AUDIOSHARE_LATENCY_DEBUG=1`.

### Compressed codec selection

Lossless profiles always use the zero-lookahead LPC codec. For the compressed
profiles (`Smart Auto` fallback, `Low-Latency Compressed`, and `Max Stability`)
the host supports two interchangeable backends:

| Backend | Wire tag | Lookahead | Notes |
| --- | --- | --- | --- |
| Pure-Rust VBR | `0x7E` | 0 ms | Default and automatic fallback |
| Opus RLD (`opus-rs`) | `0x7F` | 2.5 ms | Opt-in, bundled pure Rust |

The default is the pure-Rust VBR codec. Enable Opus with
`as-ultra host --libopus`, the desktop GUI's **Compressed codec** selector, or
the Android **Compressed Codec Module** switch (feedback codes `20`/`21`). The
choice is persisted and can be changed while streaming: every packet is
self-identifying, so the receiver switches decoders without a restart. If the
Opus encoder cannot be created or rejects a frame, the host falls back to VBR
for that packet rather than dropping audio.

Opus uses `RestrictedLowDelay` at 48 kHz with 2.5 ms stereo frames. Its 2.5 ms
lookahead is included in the client latency estimate and shown separately in the
latency HUD. Lossless, raw PCM, 24-bit, and DTX paths are unchanged. The bundled
codec is `opus-rs` 0.1.42; see
[THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md) for its BSD-3-Clause license.

The desktop application (`audioshare-gui`) provides the same host/client controls,
device discovery, per-application audio capture, encryption PIN setup, connected
client management, and live latency/jitter/FEC telemetry. Use the top-bar
`Latency Debug` toggle for the live component breakdown; the Android app exposes
the same view as a persisted `Debug` toggle on the Connect tab.

## Android

The `android/` directory is a self-contained Gradle project. The Rust core is
built as a shared library per ABI and loaded by `AudioEngineService`:

```bash
cargo ndk --platform 26 -t arm64-v8a -t x86_64 \
  -o android/app/src/main/jniLibs \
  build --release -p audioshare-android-jni

cd android
gradle assembleRelease
```

The phone uses Android's native AAudio low-latency exclusive path. The release
APK is signed with the debug signing key by the Gradle configuration so it can
be installed directly from GitHub Releases; no CI secrets are required. See
`.github/workflows/release.yml` for the full build pipeline.

## License

This project is licensed under the **GNU General Public License v3.0 only**
(`GPL-3.0-only`). See [LICENSE](LICENSE) for the full text. Third-party
components and their licenses are listed in
[THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).

It was inspired by the original audio-share project, which was distributed
under the Apache License 2.0. All code in this repository is an independent
implementation.
