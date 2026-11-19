# Third-Party Notices

AudioShare Ultra is licensed under GPL-3.0-only. It bundles the following
third-party components. Their licenses and project sources are listed below.

## Crates added for v2 production build

| Crate | Version | License | Project | Used by |
|---|---|---|---|---|
| `chacha20poly1305` | 0.10 | BSD-3-Clause OR Apache-2.0 | https://github.com/RustCrypto/AEADs | `audioshare-protocol` (RFC 8439 AEAD) |
| `pbkdf2` | 0.12 | MIT OR Apache-2.0 | https://github.com/RustCrypto/password-hashes | `audioshare-protocol` (PIN KDF) |
| `sha2` | 0.10 | MIT OR Apache-2.0 | https://github.com/RustCrypto/hashes | `audioshare-protocol` (PBKDF2-HMAC-SHA256) |
| `getrandom` | 0.2 | MIT OR Apache-2.0 | https://github.com/rust-random/getrandom | `audioshare-protocol` (per-session salt) |

These are pure-Rust crates with no C/NDK dependency. Full license texts are
available in each crate's published source package on crates.io.

## Android UI dependencies (FOSS, Apache-2.0)

| Component | Version | License | Project |
|---|---|---|---|
| CameraX (camera-camera2 / lifecycle / view) | 1.3.4 | Apache-2.0 | https://developer.android.com/jetpack/androidx/releases/camera |
| ZXing Core | 3.5.3 | Apache-2.0 | https://github.com/zxing/zxing |
| material-icons-extended / lifecycle-viewmodel-compose | BOM 2024.06.00 / 2.8.4 | Apache-2.0 | https://developer.android.com/jetpack |

## opus-rs 0.1.42

- **Project:** https://github.com/restsend/opus-rs
- **Crate:** https://crates.io/crates/opus-rs/0.1.42
- **License:** BSD-3-Clause
- **Used by:** `audioshare-protocol` (compressed-mode Opus backend)

`opus-rs` is a pure-Rust implementation of the Opus audio codec, itself derived
from the reference Opus implementation. It is compiled into the host and client
binaries and is only exercised when the user selects the optional Opus backend
for a compressed quality profile.

The full license text shipped with `opus-rs` 0.1.42 is reproduced verbatim:

```text
Copyright 2026 restsend.com

Copyright 2001-2023 Xiph.Org, Skype Limited, Octasic,
                    Jean-Marc Valin, Timothy B. Terriberry,
                    CSIRO, Gregory Maxwell, Mark Borgerding,
                    Erik de Castro Lopo, Mozilla, Amazon

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions
are met:

- Redistributions of source code must retain the above copyright
notice, this list of conditions and the following disclaimer.

- Redistributions in binary form must reproduce the above copyright
notice, this list of conditions and the following disclaimer in the
documentation and/or other materials provided with the distribution.

- Neither the name of Internet Society, IETF or IETF Trust, nor the
names of specific contributors, may be used to endorse or promote
products derived from this software without specific prior written
permission.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS
``AS IS'' AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT
LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR
A PARTICULAR PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT OWNER
OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL,
EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO,
PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE, DATA, OR
PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF
LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING
NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF THIS
SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.

Opus is subject to the royalty-free patent licenses which are
specified at:

Xiph.Org Foundation:
https://datatracker.ietf.org/ipr/1524/

Microsoft Corporation:
https://datatracker.ietf.org/ipr/1914/

Broadcom Corporation:
https://datatracker.ietf.org/ipr/1526/
```
