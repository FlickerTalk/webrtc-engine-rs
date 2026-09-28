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
| `rtp`    | Opus sobre pistas de webrtc-rs 0.21 (API sans-IO + crate `rtc`): `media_engine` (Opus y H.264), `peer_connection_builder` (informes RTCP y lo que pide el vídeo), `add_audio_track` → `AudioSender`, `AudioReceiver`. El payload type negociado se lee en cada envío. |
| `jitter` | `JitterBuffer`: reordena por secuencia (con el salto de 65535 a 0), profundidad adaptativa de 1 a 10 tramas según el jitter RFC 3550 medido con `push_at`; `playout()` → `Frame` / `Missing` / `Waiting`; `peek_next` para la FEC. |
| `call`   | La cadena. `Uplink` y `Downlink` son máquinas de estado síncronas; `Call` las ejecuta en tres tareas de Tokio (envío, recepción, reproducción). Transporte abstracto: `PacketSink` / `PacketSource`, implementados para `AudioSender`, `AudioReceiver` y `RemoteAudio`. |
| `netsim` | `NetworkSimulator`: red simulada sans-IO y determinista con semilla (retardo, jitter uniforme, pérdida, reordenación, duplicados). `simulated_link` la usa como transporte de una `Call` con el reloj de Tokio; `simulated_video_link`, como un sentido del vídeo (tramas en paquetes RTP a la ida, peticiones de keyframe a la vuelta). |
| `video`  | Contrato del vídeo: la plataforma captura **y** codifica (y decodifica **y** pinta) con su hardware; el núcleo solo mueve access units H.264 Annex-B (`EncodedFrame`, SPS/PPS en cada keyframe). `VideoSource`, `VideoSink`, `VideoConfig`, `Facing`, `Rotation` (viaja en la extensión RTP CVO, no se rota el píxel) y el canal `frame_channel` (acotado, sin bloqueo; tras descartar exige keyframe). Puntos de anclaje de iOS/Android/escritorio en la doc del módulo. |
| `video::{h264, packet, assemble}` | Núcleo del vídeo, sans-IO: unidades NAL y keyframes (IDR); `Packetizer` (NAL sola, STAP-A con SPS/PPS, FU-A a 1200 bytes, timestamp a 90 kHz desde `EncodedFrame::timestamp`, marcador y rotación en el último paquete); `FrameAssembler` (paquetes → access units Annex-B en orden de decodificación, descarte de tramas incompletas y petición de keyframe). |
| `video::bitrate` | `BitrateController`: control por pérdidas de los receiver reports (al estilo de la parte de pérdidas de GCC) con tope por REMB; función pura con el tiempo como entrada. |
| `video::call` | `VideoCall` (tres tareas de Tokio: envío, realimentación, recepción), `RemoteVideo` (paquetes → tramas, PLI), y el transporte abstracto `FrameSink` / `FeedbackSource` / `FrameSource` / `VideoPacketSource`. Independiente de la `Call` de audio. |
| `video::rtp` | H.264 sobre webrtc-rs: `h264_codec` (42e01f, modo 1), `register_h264` (+ extensión CVO), `configure_video` (NACK, transport-cc de recepción, `VideoFeedbackInterceptor`), `add_video_track` → `VideoSender`, `VideoReceiver` (= `RemoteVideo<TrackVideoPackets>`), `VideoFeedback`. |
| `video::fake` | `FakeSource` (access units sintéticos con SPS/PPS, tamaño según bitrate, número de trama dentro) y `FakeSink` (decodificador de prueba: falla con un delta sin su referencia). Para pruebas y demos. |

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

### El vídeo (`video`)

Objetivo: interoperar con Chrome, Safari y el WebView de Android en **H.264 Constrained Baseline
`42e01f`, `packetization-mode=1`**.

```text
VideoSource → FrameSender → FrameReceiver → [envío] → FrameSink (VideoSender: Packetizer → RTP)
FeedbackSource (VideoFeedback: PLI/FIR/RR/REMB) → [realimentación] → request_keyframe / set_bitrate
VideoPacketSource (TrackVideoPackets) → RemoteVideo (FrameAssembler, PLI) → [recepción] → VideoSink
```

- **Envío**: `VideoSender` empaqueta él mismo (`Packetizer` sobre el `H264Payloader` de `rtc`) y
  escribe con un `TrackLocalStaticRTP`, no con `TrackLocalStaticSample`: así el timestamp RTP sale
  de `EncodedFrame::timestamp` (90 kHz, desplazamiento aleatorio) y no de una duración, y el
  marcador y la CVO van donde tocan. Payload type e id de la CVO se leen de los parámetros del
  sender en cada trama; entre varios H.264 negociados elige modo 1 y `42e0*`.
- **CVO**: se negocia (`urn:3gpp:video-orientation`, `register_h264`) y la rotación va en el
  **último paquete de cada trama**, como libwebrtc; al recibir, sin extensión es `Deg0`. El id se
  lee del sender del mismo transceptor (una m-section, un juego de ids).
- **RTCP entrante (lo que no da webrtc-rs por defecto)**: en 0.21 el RTCP entrante se queda en
  los interceptores salvo que uno lo marque `Attribute::DeliverToApplication`, y se enruta a una
  pista por el **primer SSRC** que nombra el primer paquete del compuesto (un RR cuyo primer
  bloque es del audio nunca llegaría al vídeo). `VideoFeedbackInterceptor` (slot 14 000, después
  de todos) aprende los SSRC de vídeo locales en `bind_local_stream`, saca del compuesto los PLI,
  FIR, bloques de RR/SR y REMB que son de ellos, **uno por paquete**, y los marca; el paquete
  original sigue igual. `VideoFeedback` los lee con `TrackLocal::poll` (con timeout de 200 ms: la
  espera retiene un lock que necesita un nuevo `bind`) y los traduce a `Feedback`. RTT = llegada
  (NTP del reloj de pared) − LSR − DLSR.
- **PLI saliente**: `TrackRemote::write_rtcp` con el SSRC del paquete recibido como `media_ssrc`
  y el de nuestro vídeo como emisor (`VideoReceiver::pending`; con `from_track`, 0).
- **Reensamblado** (`FrameAssembler`): una trama está entera cuando están todos los números de
  secuencia de su primer paquete al del marcador; sale en orden de decodificación (un delta solo
  detrás de la trama anterior). Una keyframe entera se toma siempre y tira todo lo anterior (el
  inicio es seguro si el paquete anterior es de otra trama o si abre con SPS/AUD). Un hueco que
  sigue abierto `MAX_WAIT` (200 ms, tiempo para un NACK) descarta lo que espera detrás y pide
  keyframe; los deltas que llegan mientras se espera keyframe se retienen (puede llegar una
  keyframe anterior reordenada) y caen al llegar ella o por tiempo. PLI como mucho cada
  `KEYFRAME_REQUEST_INTERVAL` (500 ms) mientras se deba. Un payload corrupto (FU-A sin inicio,
  STAP-A truncado, NAL no WebRTC) descarta la trama y pide keyframe.
- **El decodificador pide keyframe**: si `VideoSink::push` devuelve error, `VideoCall` pide una
  (PLI) y el ensamblador descarta deltas hasta que llegue. No hace falta tocar el contrato.
- **Bitrate** (`BitrateController`): pérdida > 10 % → baja `×(1 − 0,5·pérdida)` (como mucho cada
  300 ms); < 2 % y RTT ≤ 500 ms → sube un 5 % (como mucho una vez por segundo); entre medias,
  se mantiene. El REMB es tope (y un REMB mayor solo sube el tope). Siempre con `clamp_bitrate`.
  De 150 kbit/s a 2,5 Mbit/s con red limpia tarda ~58 s.
- **Pausa (cámara apagada)**: se para la fuente y **no se envía nada**; al reanudar, la fuente
  arranca otra vez y se pide keyframe. El otro lado se queda con la última imagen; «cámara
  apagada» lo dice la app por su señalización. (Alternativa descartada: keyframe negro, que
  obliga a tener un codificador en el núcleo.) `switch_camera` en pausa solo cambia la cámara
  con la que se reanudará.
- **Estadísticas** (`VideoStats`): tramas enviadas/keyframes, errores, descartes del canal,
  peticiones de keyframe recibidas, bitrate, tramas mostradas, errores del sink y las del
  ensamblador (descartadas, PLI enviados, tardías, duplicadas). Sin contenido ni identificadores.
- **Interceptores de vídeo** en `peer_connection_builder`: NACK (generador y respondedor, sin RTX:
  las retransmisiones van por el SSRC original), transport-cc solo de recepción (generamos el
  feedback TWCC que usa la estimación de ancho de banda del navegador) y el de feedback.

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
- **PRNG propio (SplitMix64) en el simulador**: su salida para una semilla no cambia al actualizar
  dependencias, así las pruebas con semilla ven siempre la misma red. No es para nada secreto.

## Comandos

```sh
cargo test --all-features                          # todo; los tests con micrófono son #[ignore]
cargo test --test pipeline -- --nocapture          # cadena completa sobre el simulador
cargo test --test loopback -- --nocapture          # cadena completa sobre webrtc-rs en loopback (audio y vídeo)
cargo test --test video_pipeline -- --nocapture    # vídeo sobre el simulador (red limpia y con pérdida)
cargo clippy --all-targets --all-features -- -D warnings
cargo clippy --all-targets -- -D warnings          # sin desktop
cargo fmt --check

# Demo para oírlo en el Mac (con auriculares, si no se acopla):
cargo run --release --example call_demo --features desktop -- --loss 10 --jitter 40 --delay 50 --seconds 30
cargo run --example mic_echo --features desktop    # solo dispositivos, 200 ms de eco

# iOS
IPHONEOS_DEPLOYMENT_TARGET=15.0 cargo build --target aarch64-apple-ios

# Android
NDK_BIN=~/Library/Android/sdk/ndk/27.1.12297006/toolchains/llvm/prebuilt/darwin-x86_64/bin
CC_aarch64_linux_android=$NDK_BIN/aarch64-linux-android24-clang \
AR_aarch64_linux_android=$NDK_BIN/llvm-ar \
CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER=$NDK_BIN/aarch64-linux-android24-clang \
cargo build --target aarch64-linux-android
```

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
- **Vídeo, pendiente**: RTX (hoy las retransmisiones NACK van por el SSRC original); estimación
  de ancho de banda por transport-cc en el emisor (hoy solo pérdidas de los RR y REMB; el
  interceptor GCC de webrtc-rs sin probar); el REMB cuenta todo lo que lista (audio incluido);
  el número de secuencia del FIR no se usa para deduplicar; probar la interoperabilidad real con
  Chrome, Safari y el WebView (answer de un navegador, perfiles H.264 que ofrece, lectura de la
  CVO y del PLI con `sender_ssrc`); las fuentes y sumideros de plataforma (ramas aparte).

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
- Vídeo (rama `video-core`): núcleo común sobre el contrato `video` — empaquetado y reensamblado
  H.264 (FU-A, STAP-A), keyframes y PLI, control de bitrate, `VideoCall` con pausa, cambio de
  cámara y estadísticas, `FakeSource`/`FakeSink`, enlace de vídeo del simulador y pistas H.264 en
  webrtc-rs con CVO e interceptor de feedback. Pruebas: sobre el simulador (limpia: todas las
  tramas en orden, sin peticiones; 1 % de pérdida y reordenación: se piden keyframes, llegan a la
  cámara y la pantalla nunca muestra basura) y en loopback real (Opus y H.264 en la misma
  conexión, rotación por CVO, PLI hasta la cámara).
