# Third-Party Licenses & Notices

Home Mate (小瑞) incorporates or interoperates with the following open-source software components and model assets. We gratefully acknowledge their authors and maintainers.

## 1. Rust Crates & Runtime Libraries

| Component | Version | License | Usage & Notes |
| :--- | :--- | :--- | :--- |
| `tokio` / `axum` / `tower` / `hyper` | 1.x / 0.8.x / 0.5.x / 1.x | `MIT` | Async runtime and HTTP/WebSocket management plane |
| `serde` / `serde_json` | 1.0.x | `MIT OR Apache-2.0` | JSON / LDJSON wire serialization |
| `rusqlite` / `libsqlite3-sys` (bundled SQLite) | `0.43.0` / `0.40.0` | `MIT` / `Public Domain` | Embedded SQLite state persistence |
| `symphonia` (`core`, `mp3`, `flac`, `isomp4`, `aac`, `pcm`, `wav`) | 0.5.5 | `MPL-2.0` | Unmodified audio stream decoding library; source code available upstream at https://github.com/pdeljanov/Symphonia (MPL-2.0 §3.2(a) notice) |
| `tokio-tungstenite` / `tungstenite` | 0.30.0 | `MIT` / `MIT OR Apache-2.0` | Home Assistant WebSocket client and gateway transport |
| `rustls` / `tokio-rustls` / `hyper-rustls` | 0.23.x / 0.26.x / 0.27.x | `Apache-2.0 OR ISC OR MIT` | Pure-Rust TLS implementation |
| `ring` | 0.17.14 | `Apache-2.0 AND ISC` | Cryptographic primitives for TLS (`rustls` backend) |
| `webpki-roots` | 0.26.x / 1.0.x | `CDLA-Permissive-2.0` | Mozilla root CA certificates for pure-Rust TLS |
| `chacha20poly1305` / `aead` / `zeroize` | 0.10.1 / 0.5.2 / 1.9.1 | `Apache-2.0 OR MIT` | Vault AEAD secret sealing (`enc:v1:`) |
| `ed25519-dalek` / `curve25519-dalek` | 2.2.0 / 4.1.3 | `BSD-3-Clause` | Release manifest signature verification (`hm-release`) |
| `sha2` / `md-5` / `tar` / `reqwest` | 0.10.x / 0.10.x / 0.4.x / 0.12.x | `MIT OR Apache-2.0` | Artifact digest verification and HTTP tool client |
| `pinyin` | 0.11.0 | `MIT` | Mandarin Pinyin conversion for wake-word matching |
| `icu_*` / `unicode-*` | 1.x / 2.x | `Unicode-3.0` | Unicode normalization and IDNA/URL processing (`url` / `idna` transitive crates) |
| `tract-onnx` (optional `silero-vad` feature) | 0.21.19 | `MIT OR Apache-2.0` | Optional in-process Silero VAD inference on fat builds (excluded in `--slim` builds) |

## 2. Web Console Frontend Dependencies

| Component | Version | License | Usage |
| :--- | :--- | :--- | :--- |
| `vue` | 3.5.x | `MIT` | Reactive web console UI framework |
| `vue-router` | 4.6.4 | `MIT` | Single-page console routing |
| `lucide-vue-next` | 1.0.0 | `ISC` | Console UI vector icons |
| `entities` | `7.0.1` | `BSD-2-Clause` | HTML entity decoding (transitive Vue runtime dependency) |
| `source-map-js` | 1.x | `BSD-3-Clause` | Source-map support (transitive CSS/Vue toolchain dependency) |

## 3. On-Device Audio, Keyword Spotting & Neural VAD Assets

| Component | License | Source & Attribution |
| :--- | :--- | :--- |
| `sherpa-onnx` | `Apache-2.0` | Next-gen Kaldi Zipformer KWS & VAD C-ABI runtime (https://github.com/k2-fsa/sherpa-onnx) |
| `onnxruntime` | `MIT` | Microsoft ONNX Runtime shared library used by `hm-kws` (https://onnxruntime.ai/) |
| `silero_vad.onnx` (Silero VAD v4) | `MIT` | Copyright (c) Silero Team (https://github.com/snakers4/silero-vad). Loaded at runtime from disk |
| `speexdsp` | `BSD-3-Clause` | Acoustic Echo Cancellation (AEC) and preprocessing DSP primitives (https://github.com/xiph/speexdsp) |
| `open-xiaoai` (External prerequisite on LX06) | `MIT` | Used independently by end users to enable SSH on Xiaomi speakers (https://github.com/idootop/open-xiaoai) |
| `firebreak` / `amonet` (External prerequisite on Echo Dot 2) | `MIT` / `GPL` | Used independently by end users to unlock Echo Dot 2 Bootrom over USB (https://github.com/yes999zc/firebreak) |
