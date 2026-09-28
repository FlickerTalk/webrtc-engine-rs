# webrtc-engine

A media engine for WebRTC voice calls on top of [webrtc-rs](https://github.com/webrtc-rs/webrtc),
written for [FlickerTalk](https://flickertalk.com), a private peer-to-peer messenger.

Phones need call audio that runs natively: a call answered with CallKit on a locked iPhone has
no WebView to play it, so the WebView's WebRTC cannot carry it. This crate does the audio side
of a call in Rust: device I/O, Opus, RTP over webrtc-rs tracks and a jitter buffer.

## Status

Audio only, working end to end on the desktop and in tests:

- microphone → Opus → RTP → loopback webrtc-rs call → jitter buffer → Opus → speaker;
- packet loss is rebuilt from Opus in-band FEC when the next packet is there, and concealed
  (PLC) otherwise;
- the desktop backend (cpal) is for trying it on a computer.

Not there yet: the iOS backend (VoiceProcessingIO with the CallKit audio session), the Android
backend (AAudio, `VOICE_COMMUNICATION`) and the integration into the FlickerTalk app.

## Modules

| Module   | What it does |
| -------- | ------------ |
| `audio`  | Lock-free rings between the device callbacks and the engine; adapters that mix, resample and convert; the `AudioBackend` trait; `audio::desktop` (cpal, feature `desktop`). |
| `codec`  | Opus encoder and decoder (vendored libopus 1.6.1): 20 ms mono frames at 48 kHz, 32 kbit/s, in-band FEC, concealment and FEC recovery. |
| `rtp`    | Opus on webrtc-rs tracks: media engine, peer connection builder, `AudioSender`, `AudioReceiver`. |
| `jitter` | Reorders packets and hands out one frame per 20 ms; adaptive depth from 1 to 10 frames. |
| `call`   | The pipeline. `Uplink` (capture → Opus packets), `Downlink` (packets → jitter buffer → decode, FEC or PLC → playout, paced by the speaker) and `Call`, which runs both on Tokio tasks with mute, stop and counters. |
| `netsim` | A deterministic simulated network (delay, jitter, loss, reordering, duplication, seeded) and a simulated link usable as a call's transport. |
| `video`  | The video contract (work in progress): platform code captures and hardware-encodes, and decodes and renders; the core only moves H.264 access units (`EncodedFrame`) through `VideoSource`, `VideoSink` and a bounded frame channel. |

Every module works in one format, defined at the crate root: `SAMPLE_RATE` (48 kHz),
`FRAME_SAMPLES` (960), `FRAME_DURATION` (20 ms) and `Frame`.

## Tests

```sh
cargo test --all-features
```

Among them, `tests/pipeline.rs` sends a test voice through the whole pipeline over the simulated
network with a fake device and clock (clean, and 10 % loss with 40 ms jitter), and
`tests/loopback.rs` does it over a real webrtc-rs call between two peers on 127.0.0.1. Tests that
need a microphone are `#[ignore]`.

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
```

## Hear it: the call demo

Your microphone goes through Opus, a simulated network and the jitter buffer to your speaker,
as the other side of a call would hear you. **Use headphones**, or the speaker feeds back into
the microphone. On macOS, the terminal asks for microphone permission the first time.

```sh
cargo run --release --example call_demo --features desktop -- --loss 10 --jitter 40 --delay 50 --seconds 30
```

Every second it prints what was received, concealed and recovered by FEC, the jitter buffer
depth and the underruns. `mic_echo` is a simpler check of the devices alone:
`cargo run --example mic_echo --features desktop`.

## Cross builds

libopus is compiled with the `cc` crate, so there is no cmake to install.

```sh
# iOS
rustup target add aarch64-apple-ios
IPHONEOS_DEPLOYMENT_TARGET=15.0 cargo build --target aarch64-apple-ios

# Android (NDK r27; on Linux the prebuilt directory is linux-x86_64)
rustup target add aarch64-linux-android
NDK_BIN=$ANDROID_HOME/ndk/27.1.12297006/toolchains/llvm/prebuilt/darwin-x86_64/bin
CC_aarch64_linux_android=$NDK_BIN/aarch64-linux-android24-clang \
AR_aarch64_linux_android=$NDK_BIN/llvm-ar \
CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER=$NDK_BIN/aarch64-linux-android24-clang \
cargo build --target aarch64-linux-android
```

Leave the `desktop` feature off for phones.

## Licence

AGPL-3.0-only, see [LICENSE](LICENSE). The vendored libopus in `vendor/opus` keeps its own
BSD-3-Clause licence (`vendor/opus/COPYING`).
