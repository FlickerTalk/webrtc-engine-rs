# webrtc-engine

A media engine for WebRTC voice and video calls on top of
[webrtc-rs](https://github.com/webrtc-rs/webrtc), written for [FlickerTalk](https://flickertalk.com),
a private peer-to-peer messenger.

Phones need call audio that runs natively: a call answered with CallKit on a locked iPhone has
no WebView to play it, so the WebView's WebRTC cannot carry it. This crate does the audio side
of a call in Rust: device I/O, Opus, RTP over webrtc-rs tracks and a jitter buffer. Video
follows the same idea: the phone's hardware captures, encodes, decodes and draws; the engine
moves H.264 between them and the network.

## Status

Audio, working end to end on the desktop and in tests:

- microphone → Opus → RTP → loopback webrtc-rs call → jitter buffer → Opus → speaker;
- packet loss is rebuilt from Opus in-band FEC when the next packet is there, and concealed
  (PLC) otherwise;
- the desktop backend (cpal) is for trying it on a computer;
- the Android backend (AAudio, `VOICE_COMMUNICATION`, with the platform's echo canceller) is
  tested on a device; the app's Kotlin side must hold `RECORD_AUDIO` and set
  `MODE_IN_COMMUNICATION` before starting it (see `src/audio/android.rs`);
- `audio::ios::VoiceProcessingBackend` runs Apple's VoiceProcessingIO unit (echo cancellation,
  noise suppression, gain control). The app owns the audio session: with CallKit, it starts the
  backend from `provider(_:didActivate:)`; see the module documentation. It runs in the iOS
  simulator; not tried on a phone yet.

`audio::platform_backend()` returns the backend for the target (iOS, Android, or the desktop
with the `desktop` feature). While it runs, its owner calls `AudioBackend::maintain()` about
every 100 ms: on Android that reopens the streams after a headset is plugged in or out, and
without it the call goes silent.

Video, working end to end in tests and on the desktop; the phone backends run in the iOS
simulator and on an Android tablet, not yet in the app:

- H.264 Constrained Baseline (`42e01f`, packetization mode 1) next to Opus on the same
  webrtc-rs connection, with NACK, receive-side transport-cc and the orientation extension
  (CVO); frames are cut into RTP packets and reassembled, and a lost frame, or a decoder that
  says it needs one (`VideoSink::keyframe_needed`), asks the sender for a keyframe (PLI, rate
  limited);
- `VideoCall` runs a camera and a display against the network, with bitrate control from
  receiver reports and REMB, pause, camera switch, restart of a lost camera (`VideoSource::lost`)
  and counters;
- `video::platform_source()` and `video::platform_sink()` give the camera and the display of
  the target: VideoToolbox and `AVSampleBufferDisplayLayer` on iOS, Camera2 and MediaCodec on
  Android (capture from 8.0), OpenH264 with a window on the desktop (feature `desktop`);
- not there yet: retransmissions on their own stream (RTX), pacing and a send-side bandwidth
  estimate from transport-cc; checks against real browsers.

Not there yet: the integration into the FlickerTalk app.

## Modules

| Module   | What it does |
| -------- | ------------ |
| `audio`  | Lock-free rings between the device callbacks and the engine; adapters that mix, resample and convert; the `AudioBackend` trait; `audio::desktop` (cpal, feature `desktop`); `audio::ios` (VoiceProcessingIO, iOS only); `audio::android` (AAudio, Android only). |
| `codec`  | Opus encoder and decoder (vendored libopus 1.6.1): 20 ms mono frames at 48 kHz, 32 kbit/s, in-band FEC, concealment and FEC recovery. |
| `rtp`    | Opus on webrtc-rs tracks: media engine, peer connection builder, `AudioSender`, `AudioReceiver`. |
| `jitter` | Reorders packets and hands out one frame per 20 ms; adaptive depth from 1 to 10 frames. |
| `call`   | The pipeline. `Uplink` (capture → Opus packets), `Downlink` (packets → jitter buffer → decode, FEC or PLC → playout, paced by the speaker) and `Call`, which runs both on Tokio tasks with mute, stop and counters. |
| `netsim` | A deterministic simulated network (delay, jitter, loss, reordering, duplication, seeded), a simulated link usable as a call's transport, and `simulated_video_link` for one direction of video. |
| `video`  | The video contract: platform code captures and hardware-encodes, and decodes and renders; the core only moves H.264 access units (`EncodedFrame`) through `VideoSource`, `VideoSink` and a bounded frame channel. `platform_source()` / `platform_sink()`. `video::ios`: the camera (AVFoundation) with the VideoToolbox H.264 encoder, and remote video on an `AVSampleBufferDisplayLayer`. `video::android`: `CameraSource` (Camera2 into the MediaCodec encoder's input surface, Android 8.0+) and `DisplaySink` (MediaCodec decoder rendering straight to the app's surface). |
| `video::{h264, packet, assemble, bitrate}` | NAL units and keyframes; RTP packetisation (single NAL, STAP-A, FU-A) and reassembly into whole frames with keyframe requests; loss-based bitrate control capped by REMB. |
| `video::call` | `VideoCall`: sending, feedback and receiving on Tokio tasks; `RemoteVideo` turns RTP packets into frames. |
| `video::rtp` | H.264 on webrtc-rs tracks: `VideoSender`, `VideoReceiver`, `VideoFeedback` (PLI, FIR, receiver reports, REMB). |
| `video::fake` | `FakeSource` and `FakeSink`: synthetic frames and a strict fake decoder, for tests and demos. |
| `video::openh264` | Software H.264 (feature `openh264`, off by default) with Cisco's OpenH264: `SoftwareEncoder` (I420 → Constrained Baseline Annex-B, SPS/PPS on every keyframe, `force_keyframe`, `set_bitrate` while running), `SoftwareDecoder` (Annex-B → I420, drops delta frames until a keyframe after an error), and `I420Frame` with RGB(A) conversion and a moving `test_pattern`. For tests and the desktop. |
| `video::desktop` | Desktop video (feature `desktop`, macOS): `CameraSource` (camera through nokhwa/AVFoundation → OpenH264, on its own thread), `WindowSink` (OpenH264 → a `FrameSlot`) and `VideoWindow` (a minifb window that must live on the main thread). |

Every module works in one format, defined at the crate root: `SAMPLE_RATE` (48 kHz),
`FRAME_SAMPLES` (960), `FRAME_DURATION` (20 ms) and `Frame`.

## Tests

```sh
cargo test --all-features
```

Among them, `tests/pipeline.rs` sends a test voice through the whole pipeline over the simulated
network with a fake device and clock (clean, and 10 % loss with 40 ms jitter), and
`tests/loopback.rs` does it over a real webrtc-rs call between two peers on 127.0.0.1, with
audio and video on the same connection. `tests/video_pipeline.rs` runs two video calls against
each other over the simulator (clean, and with loss and reordering). Tests that need a
microphone, a camera or a window are `#[ignore]`; the iOS and Android backends have more that
run in the simulator or on a device.

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

## See it: the video demo

Your Mac's camera is encoded with OpenH264 and goes through a `VideoCall`: cut into RTP
packets, across a simulated network (delay, jitter, loss), reassembled, decoded again and shown
in a window, with your self-preview in the corner. A lost packet makes the receiver ask the
camera for a keyframe. The first time, macOS asks for camera permission for the terminal.

```sh
cargo run --release --example video_demo --features desktop -- --loss 5 --jitter 30 --delay 50
```

Flags: `--loss PERCENT`, `--jitter MS`, `--delay MS`, `--bitrate KBITS` (800 by default),
`--seconds S` (by default it runs until you close the window or press Escape) and
`--source camera|fake`. With `--source fake` there is no camera and no window: synthetic frames
run through the same call for 10 seconds (or `--seconds`) and only the counters are printed.
Every second it prints the frames sent and received per second, the bitrate, keyframes, frames
dropped, keyframe requests and decode errors. Use `--release`: software H.264 is slow in debug
builds.

## Cross builds

libopus is compiled with the `cc` crate, and so is OpenH264 (by the `openh264` crate), so there
is no cmake to install; on arm64 OpenH264 needs no nasm either.

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

Leave the `desktop` feature off for phones. The Android device tests are `#[ignore]`: build them
with `cargo test --target aarch64-linux-android --lib --no-run`, push the test binary to
`/data/local/tmp` and run it there with `--ignored audio::android` or `--ignored video::android`.
The iOS ones run in the simulator: `cargo test --target aarch64-apple-ios-sim --lib --no-run`,
then `xcrun simctl spawn booted <test binary> --ignored video::ios`.

## Licence

AGPL-3.0-only, see [LICENSE](LICENSE). The vendored libopus in `vendor/opus` keeps its own
BSD-3-Clause licence (`vendor/opus/COPYING`).

The optional features pull in more code under its own licence: OpenH264 (feature `openh264`,
BSD-2-Clause; built from source, which is **not** covered by the H.264 patent licence Cisco
pays for its prebuilt binaries, so it is meant for tests and trying things on a computer),
nokhwa (Apache-2.0) and minifb (MIT or Apache-2.0) for `desktop`.
