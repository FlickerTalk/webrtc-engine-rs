# webrtc-engine-rs

Motor de medios para las llamadas de FlickerTalk sobre webrtc-rs (repo `FlickerTalk/webrtc-engine-rs`,
AGPL-3.0-only, edición 2024). Objetivo: el audio de la llamada nativo en iOS y Android, para que
una llamada contestada con CallKit con el iPhone bloqueado tenga audio; el WebRTC del WebView no
puede. Primero audio; el vídeo, después. El escritorio (macOS) es para probarlo y oírlo.

## Arquitectura

Un solo formato en todo el crate, definido en la raíz (`lib.rs`): 48 kHz, mono, i16, tramas de
20 ms (`SAMPLE_RATE`, `FRAME_SAMPLES` = 960, `FRAME_DURATION`, `Frame`).

```text
micrófono → callback → CaptureAdapter → anillo → CaptureFrames → Uplink (Opus) → PacketSink
PacketSource → Downlink: JitterBuffer → Opus (decode / FEC / PLC) → PlayoutFrames → anillo
            → PlayoutAdapter → callback → altavoz
```

| Módulo   | Qué hace |
| -------- | -------- |
| `audio`  | Anillos SPSC sin bloqueos ni asignaciones entre los callbacks del dispositivo y el motor; adaptadores (mezcla a mono, remuestreo lineal, i16/f32, silencio y cuenta de vacíos); trait `AudioBackend`; `audio::desktop` con cpal (feature `desktop`, desactivada por defecto). |
| `codec`  | Opus con libopus 1.6.1 vendorizada en `vendor/opus` y compilada con `cc` (`build.rs`, sin cmake). VoIP, 32 kbit/s, FEC en banda, 10 % de pérdida esperada, DTX apagado. `decode`, `conceal` (PLC), `recover` (FEC del paquete siguiente). |
| `rtp`    | Opus sobre pistas de webrtc-rs 0.21 (API sans-IO + crate `rtc`): `media_engine`, `peer_connection_builder`, `add_audio_track` → `AudioSender`, `AudioReceiver`. El payload type negociado se lee en cada envío. |
| `jitter` | `JitterBuffer`: reordena por secuencia (con el salto de 65535 a 0), profundidad adaptativa de 1 a 10 tramas según el jitter RFC 3550 medido con `push_at`; `playout()` → `Frame` / `Missing` / `Waiting`; `peek_next` para la FEC. |
| `call`   | La cadena. `Uplink` y `Downlink` son máquinas de estado síncronas; `Call` las ejecuta en tres tareas de Tokio (envío, recepción, reproducción). Transporte abstracto: `PacketSink` / `PacketSource`, implementados para `AudioSender`, `AudioReceiver` y `RemoteAudio`. |
| `netsim` | `NetworkSimulator`: red simulada sans-IO y determinista con semilla (retardo, jitter uniforme, pérdida, reordenación, duplicados). `simulated_link` la usa como transporte de una `Call` con el reloj de Tokio. |
| `video`  | Contrato del vídeo: la plataforma captura **y** codifica (y decodifica **y** pinta) con su hardware; el núcleo solo mueve access units H.264 Annex-B (`EncodedFrame`, SPS/PPS en cada keyframe). `VideoSource`, `VideoSink`, `VideoConfig`, `Facing`, `Rotation` (viaja en la extensión RTP CVO, no se rota el píxel) y el canal `frame_channel` (acotado, sin bloqueo; tras descartar exige keyframe). Puntos de anclaje de iOS/Android/escritorio en la doc del módulo. |
| `video::openh264` | H.264 por software (feature `openh264`, desactivada por defecto) con OpenH264 de Cisco (crate `openh264` 0.9, BSD-2-Clause). `SoftwareEncoder`: I420 → Annex-B Constrained Baseline (`42c0xx`), SPS/PPS en cada keyframe, keyframes solo a petición (`force_keyframe`), `set_bitrate` en marcha sin keyframe (`ENCODER_OPTION_BITRATE`, único `unsafe`). `SoftwareDecoder`: Annex-B → I420; tras un error (`needs_keyframe`) descarta los deltas hasta el siguiente IDR. `I420Frame` (BT.601 rango limitado) con conversión RGB(A), `rgb_at` y `test_pattern` (imagen sintética en movimiento, para pruebas de otros módulos). |
| `video::desktop` | Vídeo de escritorio (feature `desktop`, que activa `openh264`; macOS). `CameraSource` (`VideoSource`): nokhwa/AVFoundation en YUYV → I420 → `SoftwareEncoder` → `FrameSender`, en su hilo; permiso de cámara, `request_keyframe`, `set_bitrate`, `switch_camera` (la siguiente cámara o `Unsupported`) y vista previa en un `FrameSlot`. `WindowSink` (`VideoSink`): decodifica en `push` y deja la imagen en un `FrameSlot`, con `SinkMonitor` (contadores y «falta keyframe»). `VideoWindow` (minifb) pinta el remoto encajado y girado y la vista previa en espejo; **solo en el hilo principal**. |

### La cadena de la llamada (`call`)

- **Envío**: cada tick (5 ms) la tarea lee las tramas enteras del anillo de captura, las codifica
  y manda un paquete por trama. **Silenciar** envía silencio codificado, no deja de enviar: un
  hueco en los paquetes lo leería el otro lado como pérdida o reinicio.
- **Recepción**: la tarea de recepción sella la hora de llegada con un reloj monotónico
  (`tokio::time::Instant`) y pasa `(paquete, llegada)` por un canal acotado a la de reproducción,
  que los mete en el jitter buffer con `push_at`.
- **Reproducción al ritmo del altavoz**: `Downlink::pump` solo saca una trama del jitter buffer
  cuando el anillo de reproducción baja de `playout_queue_frames` (2) tramas. Manda el reloj del
  dispositivo, no un temporizador, así que no hay deriva entre relojes.
  - `Frame` → `decode`; un paquete vacío o que no sea de 20 ms → error contado y ocultación.
  - `Missing` → `recover(siguiente)` si el siguiente ya está en el buffer (`peek_next`); si no,
    `conceal`.
  - `Waiting` antes de la primera trama → silencio; `Waiting` en plena llamada → `conceal` y se
    cuenta como vacío (`underruns`).
- **Parada**: `Call::stop` avisa por un `watch`, espera a las tres tareas y devuelve las
  estadísticas finales; soltar la `Call` sin `stop` aborta las tareas. Primero se para el
  dispositivo (`AudioBackend::stop`) y después la llamada.
- **Estadísticas** (`CallStats`): solo contadores (codificadas, silenciadas, enviadas, errores,
  recibidas, decodificadas, recuperadas por FEC, ocultadas, vacíos, tardías, duplicadas,
  descartadas, profundidad, objetivo, jitter). Nunca contenido de audio ni identificadores.
- **`RemoteAudio`**: webrtc-rs anuncia la pista remota (`on_track`) al llegar su primer paquete
  RTP, o sea, después de empezar a enviar. La llamada arranca con `RemoteAudio` y su primer
  `recv` espera la pista por un `oneshot`.

## Decisiones y por qué

- **libopus vendorizada y compilada con `cc`**: los crates que la traen usan cmake, que no está en
  las máquinas de compilación; así la compilación cruzada a iOS y Android solo necesita clang.
- **Un formato único (48 kHz mono 20 ms)**: es el nativo de Opus; el remuestreo se hace una vez,
  en los adaptadores del dispositivo.
- **Marcapasos en el lado del dispositivo** y cola corta (2 tramas) para el altavoz: todo lo que
  espera en el anillo es retardo que el jitter buffer no puede adaptar.
- **Núcleo síncrono + tareas finas**: `Uplink`/`Downlink`/`NetworkSimulator` no conocen el reloj;
  las pruebas los mueven a mano con reloj falso (rápidas y deterministas) y `Call` los mueve con
  Tokio.
- **Los anillos se sondean cada 5 ms**: no tienen señal de aviso (a propósito: el callback de
  audio no debe despertar a nadie).
- **Silencio al silenciar** en vez de dejar de enviar (ver arriba).
- **OpenH264 con el crate `openh264` (BSD-2-Clause)**: compila el C++ de Cisco con `cc`, sin
  cmake (comprobado: cmake no está instalado) y, en arm64, sin nasm (usa los `.S` NEON con clang;
  en x86_64 sin nasm se queda sin ensamblador, en silencio). También compila para
  `aarch64-apple-ios`. **Ojo**: la licencia de patentes H.264 que paga Cisco cubre solo sus
  binarios, no una compilación desde el código fuente: esto es para pruebas y escritorio, no para
  distribuir en los teléfonos (que usan el códec del hardware).
- **Cámara con nokhwa 0.10 (Apache-2.0), backend AVFoundation, sin `default-features`** (sin
  decodificador MJPEG): bindings de Objective-C sin cmake ni bindgen. Se le pide YUYV y el búfer
  se trata como `yuvs` (Y0 Cb Y1 Cr, rango de vídeo) con el stride deducido del tamaño del búfer:
  nokhwa etiqueta mal los formatos en macOS (manda `GRAY` fijo y llama YUYV a `420v`/`420f`).
  La conversión a I420 es nuestra y está probada.
- **Ventana con minifb (MIT/Apache-2.0)**: la opción más simple (un búfer `u32` y
  `update_with_buffer`), compila su Objective-C con `cc`. En macOS AppKit exige el hilo
  principal: `VideoWindow` no es `Send` y la demo corre su bucle en `main`; la cámara, la red y
  el decodificador le pasan las imágenes por `FrameSlot` (solo la última; nunca hace cola).
- **El decodificador descarta deltas tras un error** hasta el siguiente IDR (en vez de pintar
  basura) y lo dice (`needs_keyframe` / `SinkStats::keyframe_needed`) para pedir un PLI.
- **PRNG propio (SplitMix64) en el simulador**: su salida para una semilla no cambia al actualizar
  dependencias, así las pruebas con semilla ven siempre la misma red. No es para nada secreto.

## Comandos

```sh
cargo test --all-features                          # todo; los tests con micrófono son #[ignore]
cargo test --test pipeline -- --nocapture          # cadena completa sobre el simulador
cargo test --test loopback -- --nocapture          # cadena completa sobre webrtc-rs en loopback
cargo clippy --all-targets --all-features -- -D warnings
cargo clippy --all-targets -- -D warnings          # sin desktop
cargo fmt --check

# Demo para oírlo en el Mac (con auriculares, si no se acopla):
cargo run --release --example call_demo --features desktop -- --loss 10 --jitter 40 --delay 50 --seconds 30
cargo run --example mic_echo --features desktop    # solo dispositivos, 200 ms de eco

# Vídeo en el Mac: cámara → OpenH264 → red simulada → OpenH264 → ventana, con vista previa.
# Pide permiso de cámara para el terminal la primera vez; Escape o cerrar la ventana para salir.
cargo run --release --example video_demo --features desktop -- --loss 5 --jitter 30 --delay 50
cargo test --features openh264 --lib video::openh264 -- --nocapture   # PSNR y bitrate medidos
cargo test --features desktop --lib video::desktop -- --ignored captures_encoded_frames_from_the_camera

# iOS
IPHONEOS_DEPLOYMENT_TARGET=15.0 cargo build --target aarch64-apple-ios

# Android
NDK_BIN=~/Library/Android/sdk/ndk/27.1.12297006/toolchains/llvm/prebuilt/darwin-x86_64/bin
CC_aarch64_linux_android=$NDK_BIN/aarch64-linux-android24-clang \
AR_aarch64_linux_android=$NDK_BIN/llvm-ar \
CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER=$NDK_BIN/aarch64-linux-android24-clang \
cargo build --target aarch64-linux-android
```

La prueba de ventana (`shows_the_test_pattern_in_a_window`, `#[ignore]`) no puede pasar en macOS
con libtest, que no ejecuta en el hilo principal: la ventana se comprueba con `video_demo`.

Las pruebas de `build.rs` no las ejecuta Cargo; el comando está en el propio `build.rs`.

CI (`.github/workflows/ci.yml`): en macOS fmt, clippy con y sin `desktop`, tests y build de iOS;
en Ubuntu, build de Android con el NDK del runner (sin `desktop`, que pediría las cabeceras de
ALSA). Sin secretos.

## Reglas

- **TDD estricto**: primero el test que falla por la razón correcta (con un stub vacío si hace
  falta, no por un nombre que no compila), luego el código mínimo, luego refactor. Commits
  pequeños. El hook global exige que un `.rs` nuevo nazca con su `#[cfg(test)] mod tests`
  (ejemplos incluidos).
- El primer test del jitter buffer, `gives_packets_back_in_order`, lo escribió Ioan: **no se
  toca**.
- Código, identificadores, comentarios y nombres de test en **inglés**; comentarios pocos y del
  *porqué*. Documentación en **español**, salvo el `README.md` (inglés, para terceros).
- Commits en **español**, sin `Co-Authored-By`.
- **Nunca** se registra contenido de audio ni identificadores (ni en logs ni en estadísticas).
- Sin `unwrap`/`expect` en el código de la biblioteca.
- `cargo clippy --all-targets --all-features -- -D warnings` y `cargo fmt --check` limpios, y las
  builds de `aarch64-apple-ios` y `aarch64-linux-android` (sin `desktop`) compilando.
- Privacidad en la red: ninguna petición lleva datos personales. Si hace falta un User-Agent:
  `webrtc-engine-rs (https://github.com/FlickerTalk/webrtc-engine-rs)`.
- No ejecutar nada en los teléfonos o tabletas conectados sin que Ioan lo pida.

## Pendiente

- **Backend iOS**: VoiceProcessingIO (cancelación de eco) y la sesión de audio de CallKit.
- **Backend Android**: AAudio con `VOICE_COMMUNICATION` (y el AEC de la plataforma).
- **Integración en la app de FlickerTalk** (`app/`): sustituir el audio del WebView en las
  llamadas; señalización y ciclo de vida de la llamada.
- **DTX y marcas de tiempo RTP en el jitter buffer**: hoy supone 20 ms por secuencia; con DTX los
  silencios parecerían jitter. Hay que pasarle el timestamp RTP.
- **NEON para libopus** en arm64 (hoy se compila sin SIMD).
- **Extensiones de cabecera RTP**: audio-level y transport-cc.
- **Bit de marcador** al empezar a hablar / tras un silencio.
- Notas del jitter buffer: al drenar exceso descarta una trama real por segundo mientras baja el
  retardo.
- Paquetes de otras duraciones (10, 40, 60 ms): hoy se tratan como error y se ocultan.
- **`video_demo` sobre `VideoCall`**: hoy usa su propio enlace (`FrameLink`: una trama entera por
  paquete simulado, reordenación con 80 ms de espera, pérdida → keyframe). Cuando se fusione
  `VideoCall` (`VideoSender`/`VideoReceiver`), pasarla a `VideoCall` sobre `simulated_link`
  (hay un `TODO` en la demo).
- nokhwa 0.10 arrastra `block` 0.1.6, que Rust avisa que dejará de compilar
  (`future-incompat`); si llega a romper, sustituir nokhwa por `objc2-av-foundation`.
- La demo de vídeo aún no la ha visto nadie con la cámara: falta que Ioan la ejecute.

## Estado (2026-09-28)

Fusionadas en `engine` las cuatro ramas revisadas (`jitter`, `codec`, `rtp`, `audio`), con las
constantes unificadas en la raíz del crate y `Cargo.lock` versionado. Hecho encima:

- `call`: `Uplink`, `Downlink` (FEC con `peek_next`, PLC, silencio inicial, vacíos contados),
  `Call` con tareas de Tokio, silencio al silenciar, `stop` y estadísticas; `RemoteAudio`.
- `netsim`: simulador determinista y enlace asíncrono para `Call`.
- Pruebas de extremo a extremo: sobre el simulador con dispositivo y reloj falsos (red limpia:
  correlación > 0,9 en cada ventana, sin ocultación ni vacíos, latencia ~126 ms con 40 ms de red;
  10 % de pérdida y 40 ms de jitter: correlación media ~0,89, FEC y PLC actúan, vacíos < 5 %);
  una `Call` sobre el enlace simulado; y voz en los dos sentidos por una llamada webrtc-rs real en
  loopback (correlación ~0,97, ~86 ms).
- Demo `call_demo` (feature `desktop`) para oírlo en el Mac; compila, falta que Ioan la escuche.
- CI en `.github/workflows/ci.yml`.
- Vídeo por software y de escritorio (rama `video-desktop`): `video::openh264` (codificador y
  decodificador OpenH264; VGA a 800 kbit/s con la imagen de prueba: PSNR de luma ≥ 34 dB en las
  30 primeras tramas; 150 kbit/s → ~149 kbit/s medidos, 2 Mbit/s → ~660 kbit/s, sin keyframe al
  cambiar; una trama perdida da error y luego nada hasta el keyframe, que recupera ≥ 30 dB),
  `video::desktop` (cámara, sumidero y ventana) y la demo `video_demo`, que compila pero falta
  ver con la cámara.
