# AudioShare Ultra

**Stream PC audio to Android and use your phone as a wireless or USB PC microphone — ultra-low-latency, lossless, and open source for Windows, Linux, and Android.**

[![Release](https://img.shields.io/github/v/release/esconer/audioshare-ultra?include_prereleases)](https://github.com/esconer/audioshare-ultra/releases)
[![License: GPL v3](https://img.shields.io/badge/License-GPLv3-blue.svg)](LICENSE)
![Platforms](https://img.shields.io/badge/Platforms-Windows%20%7C%20Linux%20%7C%20Android-2ea44f)
![Built with Rust](https://img.shields.io/badge/Core-Pure%20Rust-orange)

AudioShare Ultra is a real-time wireless and wired audio bridge written in Rust. Turn any Android phone (or secondary PC) into a **low-latency wireless speaker** or **PC microphone** over Wi-Fi, USB ADB, or USB tethering—with no accounts, no ads, and no kernel drivers required.

> Inspired by the original [audio-share](https://github.com/mkckr0/audio-share) project, AudioShare Ultra is an independent Rust reimplementation built for deterministic low latency, 2.4 GHz Wi-Fi scan resilience, studio lossless audio, and cross-platform reliability.

<!-- Add a side-by-side screenshot of Desktop GUI + Android App here -->
<!-- ![AudioShare Ultra Desktop and Android](docs/screenshots/hero.png) -->

## Download & Quick Start

1. Download the latest **Windows**, **Linux**, and **Android APK** binaries from [GitHub Releases](https://github.com/esconer/audioshare-ultra/releases).
2. Start **AudioShare Ultra** on your PC and click **Start Host** (allow UDP ports `65530–65532` if prompted by your firewall).
3. Open the Android app on the same Wi-Fi network (or plug in via USB), tap your PC or scan the **QR code**, and stream instantly.

## Key Features

- **Bit-Perfect Lossless & High-Bitrate Opus Audio** — Zero-lookahead causal 2nd-order LPC (16-bit and true 24-bit studio master) plus bundled pure-Rust **Opus RLD** (`RestrictedLowDelay`, 2.5 ms frames up to **510 kbps**) with automatic per-packet switching and internal VBR fallback.
- **Built for Real 2.4 GHz Wi-Fi & Bluetooth** — `[N-1]`/`[N-2]` forward error correction (Opus RED & ADPCM FEC), WSOLA/CELT packet-loss concealment with raised-cosine crossfades, and Windows 60-second background Wi-Fi scan suppression (`WlanOpenHandle`).
- **Studio Clock-Drift Immunity** — Error-relative PI clock loop and a **20-tap Kaiser-windowed sinc fractional resampler** (`44.1 / 48 / 96 kHz`) built for multi-hour sessions without pitch drift or XRuns.
- **Phone-to-PC Virtual Microphone (Wi-Fi & USB)** — Stream your Android hardware mic to Windows (auto-detects **VB-Cable** endpoints) or Linux (PipeWire/PulseAudio) over UDP or USB ADB (`port 65532`) with automatic sink recovery.
- **Per-App Audio Isolation** — Stream a single game, browser, or music app instead of full system audio using Windows CoreAudio Process Loopback or Linux PipeWire/PulseAudio routing.
- **Multi-Device Sync & Party Mode** — Stream from one PC to multiple phones simultaneously, or broadcast internal Android audio directly from one phone to nearby phones with user-locked codecs.
- **Dual Wired USB Paths** — Zero-touch USB ADB (`TCP_NODELAY` tunnel with auto-reconnect) and zero-ADB USB Tethering (RNDIS/NCM) alongside Wi-Fi.
- **Authenticated Encryption & Per-Host PINs** — Optional PIN/QR pairing with RFC 8439 **ChaCha20-Poly1305** AEAD (65,536-round PBKDF2-SHA256, 30-minute session salt rotation, monotonic anti-replay feedback) and automatic per-host PIN memory on Android.
- **Native Low-Latency Clients** — Android Jetpack Compose app with AAudio Exclusive MMAP (plus Dolby shared-mixer and `PCM_I16`/mono fallbacks), Linux Bluetooth video lip-sync compensation (`pw-cli` / `pactl`), native `egui` desktop GUI, and `as-ultra` CLI.
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

# Optional zero-lookahead VBR fallback (Opus RLD is the default compressed codec)
as-ultra host --vbr

# Encrypted session + automatic virtual speaker routing
as-ultra host --virtual-speaker --pin 4829
as-ultra client <host-ip> --pin 4829

# Run the built-in packet-loss/latency benchmark
as-ultra benchmark

# Deterministic network-emulator soak test (+-200 ppm drift + jitter + loss)
as-ultra soak --seconds 60 --drift-ppm -200 --loss-pct 3 --opus
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
| Opus RLD (`opus-rs`) | `0x7F` | 2.5 ms | Default for compressed profiles (bundled pure Rust) |
| Pure-Rust VBR | `0x7E` | 0 ms | Zero-lookahead fallback (`--vbr`) |

The default is the pure-Rust Opus RLD codec. Select VBR with
`as-ultra host --vbr`, the desktop GUI's **Compressed codec** selector, or
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

The phone uses Android's native AAudio low-latency exclusive path. Release
builds enable R8 minification and resource shrinking, with JNI entry points kept
by `proguard-rules.pro`. They are signed with an env-injected keystore
(`AUDIOSHARE_KEYSTORE_PATH` / `PASSWORD` / `KEY_ALIAS` / `KEY_PASSWORD`) when
those values are supplied; otherwise the Gradle config falls back to the debug
key so local builds remain installable. `.github/workflows/release.yml` decodes
the base64 keystore secret, builds all ABIs with `cargo-ndk`, gates on
`cargo test --workspace`, and publishes the APK.

## License

This project is licensed under the **GNU General Public License v3.0 only**
(`GPL-3.0-only`). See [LICENSE](LICENSE) for the full text. Third-party
components and their licenses are listed in
[THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).

It was inspired by the original audio-share project, which was distributed
under the Apache License 2.0. All code in this repository is an independent
implementation.
