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
| `audio`  | Anillos SPSC sin bloqueos ni asignaciones entre los callbacks del dispositivo y el motor; adaptadores (mezcla a mono, remuestreo lineal, i16/f32, silencio y cuenta de vacíos); trait `AudioBackend`; `audio::desktop` con cpal (feature `desktop`, desactivada por defecto); `audio::ios` con `VoiceProcessingBackend` (solo iOS); `audio::android` con `AaudioBackend` sobre AAudio (solo Android); `platform_backend()` y `AudioBackend::maintain`. |
| `codec`  | Opus con libopus 1.6.1 vendorizada en `vendor/opus` y compilada con `cc` (`build.rs`, sin cmake). VoIP, 32 kbit/s, FEC en banda, 10 % de pérdida esperada, DTX apagado. `decode`, `conceal` (PLC), `recover` (FEC del paquete siguiente). |
| `rtp`    | Opus sobre pistas de webrtc-rs 0.21 (API sans-IO + crate `rtc`): `media_engine` (Opus y H.264), `peer_connection_builder` (informes RTCP y lo que pide el vídeo), `add_audio_track` → `AudioSender`, `AudioReceiver`. El payload type negociado se lee en cada envío. |
| `jitter` | `JitterBuffer`: reordena por secuencia (con el salto de 65535 a 0), profundidad adaptativa de 1 a 10 tramas según el jitter RFC 3550 medido con `push_at`; `playout()` → `Frame` / `Missing` / `Waiting`; `peek_next` para la FEC. |
| `call`   | La cadena. `Uplink` y `Downlink` son máquinas de estado síncronas; `Call` las ejecuta en tres tareas de Tokio (envío, recepción, reproducción). Transporte abstracto: `PacketSink` / `PacketSource`, implementados para `AudioSender`, `AudioReceiver` y `RemoteAudio`. |
| `netsim` | `NetworkSimulator`: red simulada sans-IO y determinista con semilla (retardo, jitter uniforme, pérdida, reordenación, duplicados). `simulated_link` la usa como transporte de una `Call` con el reloj de Tokio; `simulated_video_link`, como un sentido del vídeo (tramas en paquetes RTP a la ida, peticiones de keyframe a la vuelta). |
| `video`  | Contrato del vídeo: la plataforma captura **y** codifica (y decodifica **y** pinta) con su hardware; el núcleo solo mueve access units H.264 Annex-B (`EncodedFrame`, SPS/PPS en cada keyframe). `VideoSource`, `VideoSink`, `VideoConfig`, `Facing`, `Rotation` (viaja en la extensión RTP CVO, no se rota el píxel) y el canal `frame_channel` (acotado, sin bloqueo; tras descartar exige keyframe). Puntos de anclaje de iOS/Android/escritorio en la doc del módulo. `video::ios` (solo iOS) y `video::android` (solo Android): `CameraSource` y `DisplaySink`, ver abajo. |
| `video::{h264, packet, assemble}` | Núcleo del vídeo, sans-IO: unidades NAL y keyframes (IDR); `Packetizer` (NAL sola, STAP-A con SPS/PPS, FU-A a 1200 bytes, timestamp a 90 kHz desde `EncodedFrame::timestamp`, marcador y rotación en el último paquete); `FrameAssembler` (paquetes → access units Annex-B en orden de decodificación, descarte de tramas incompletas y petición de keyframe). |
| `video::bitrate` | `BitrateController`: control por pérdidas de los receiver reports (al estilo de la parte de pérdidas de GCC) con tope por REMB; función pura con el tiempo como entrada. |
| `video::call` | `VideoCall` (tres tareas de Tokio: envío, realimentación, recepción), `RemoteVideo` (paquetes → tramas, PLI), y el transporte abstracto `FrameSink` / `FeedbackSource` / `FrameSource` / `VideoPacketSource`. Independiente de la `Call` de audio. |
| `video::rtp` | H.264 sobre webrtc-rs: `h264_codec` (42e01f, modo 1), `register_h264` (+ extensión CVO), `configure_video` (NACK, transport-cc de recepción, `VideoFeedbackInterceptor`), `add_video_track` → `VideoSender`, `VideoReceiver` (= `RemoteVideo<TrackVideoPackets>`), `VideoFeedback`. |
| `video::fake` | `FakeSource` (access units sintéticos con SPS/PPS, tamaño según bitrate, número de trama dentro) y `FakeSink` (decodificador de prueba: falla con un delta sin su referencia). Para pruebas y demos. |
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

### Backend iOS (`audio::ios`)

`VoiceProcessingBackend` (solo `target_os = "ios"`, `Send`): `new() -> Result<Self, AudioError>`
crea la unidad `VoiceProcessingIO` (cancelación de eco, supresión de ruido y control de ganancia
del sistema), `start(DeviceIo)` la configura, la inicializa y la arranca, `stop()` la para.
Micrófono en el bus 1 (entrada habilitada), altavoz/auricular en el bus 0. Contadores:
`capture_dropped`, `playout_underruns`, `capture_errors`.

- **Contrato con Swift y CallKit** (también en la doc del módulo): **Rust nunca configura ni
  activa `AVAudioSession`.**
  1. El Swift de la app pone la categoría `.playAndRecord` y el modo `.voiceChat` antes de
     reportar o contestar la llamada; no llama a `setActive(true)`: lo activa CallKit.
  2. En `provider(_:didActivate:)` la app llama a `start`. Antes no: sin sesión activa que pueda
     grabar, la unidad se inicializa pero no arranca (OSStatus -66637, visto en el simulador).
  3. En `provider(_:didDeactivate:)` y al colgar, `stop`. Tras una interrupción, cuando CallKit
     vuelve a activar la sesión: `stop` y `start` otra vez.
  4. `Info.plist`: `NSMicrophoneUsageDescription` y los modos de fondo `audio` y `voip`. Una sola
     unidad de voz por proceso: el WebView no debe tener el micrófono durante una llamada nativa.
  5. Las rutas (altavoz, auricular, Bluetooth) son cosa de la sesión; la unidad las sigue.
- **Formatos**: pide 48 kHz mono i16 a los dos lados (el formato del motor, copia exacta); si la
  unidad no lo acepta, f32. Tras `AudioUnitInitialize` relee lo que da de verdad y se lo pasa a
  `CaptureAdapter`/`PlayoutAdapter`. En el simulador da 48 kHz mono i16.
- **Tiempo real**: los callbacks no bloquean, no reservan memoria ni hacen llamadas al sistema
  salvo `AudioUnitRender`. El búfer del micrófono se reserva al arrancar para el máximo de
  frames por callback, que se sube a 4096 (lo que usa iOS con la pantalla bloqueada). Los fallos
  en el callback se cuentan, no se propagan.
- **Parada**: `stop` para y desinicializa la unidad y **después** suelta el estado de los
  callbacks; si `AudioOutputUnitStop` falla, el estado se queda hasta que `Drop` destruye la
  unidad (`AudioComponentInstanceDispose`), así nunca se libera nada con el hilo de audio vivo.
- **FFI a mano**: una docena de declaraciones de AudioToolbox copiadas de las cabeceras del SDK,
  con tests que fijan el tamaño de cada struct. `coreaudio-sys` pediría bindgen y libclang al
  compilar; `objc2-audio-toolbox`, una familia de crates generados. Ninguna dependencia nueva.
- **Pruebas**: el formato, la traducción de `OSStatus` y el manejo de búferes de los dos
  callbacks son funciones puras que se prueban en el host (`cfg(any(target_os = "ios", test))`).
  Lo que necesita la unidad real son tests `#[ignore]` para el simulador; hacen de lado Swift y
  activan la sesión con el runtime de Objective-C (solo en los tests).

### Backend Android (`audio::android`)

- **`AaudioBackend::new()`** carga AAudio; implementa `AudioBackend` (`start(DeviceIo)`, `stop`) y
  es `Send`. Además: `voice_processing()`, `session_id()`, `capture_dropped()`,
  `playout_underruns()`, `stream_errors()` y `restart_if_needed()`.
- **Flujos**: los dos compartidos y de baja latencia, pidiendo 48 kHz mono i16; se lee lo que
  concede AAudio y los adaptadores convierten el resto (i16 o float, cualquier frecuencia y
  canales). Entrada con el preset `VOICE_COMMUNICATION` (AEC y supresión de ruido de la
  plataforma); salida con uso `VOICE_COMMUNICATION` y contenido voz, búfer de dos ráfagas. La
  entrada abre con una sesión nueva (`AAUDIO_SESSION_ID_ALLOCATE`) y la salida se une a esa
  sesión para que el AEC las empareje. Con sesión no hay MMAP: algo más de latencia.
- **AAudio se carga con `dlopen`, no se enlaza**: la app tiene minSdk 24 y AAudio llega con la
  API 26; enlazar `libaaudio.so` impediría cargar la biblioteca nativa entera en Android 7. En la
  API 24–25 `new()` devuelve `AudioError`. En la 26–27 no hay presets, uso ni sesiones: la llamada
  funciona sin el procesado de voz de la plataforma y `voice_processing()` devuelve `false`.
- **Tiempo real**: los callbacks de datos solo mueven muestras entre el búfer de AAudio y los
  anillos a través de los adaptadores (sin bloqueos ni asignaciones). El callback de error
  (dispositivo desconectado, auriculares que se enchufan o desenchufan) solo cuenta y levanta una
  bandera: AAudio prohíbe cerrar el flujo desde él. **Quien tenga el backend llama a
  `AudioBackend::maintain()` cada ~100 ms**, que aquí es `restart_if_needed()`: cierra los dos
  flujos y los reabre sobre los mismos anillos; si falla, se queda con los anillos y lo
  reintenta en la siguiente llamada.
- **Parada**: `stop` y `Drop` paran y cierran los dos flujos antes de soltar los adaptadores y
  los anillos (el `Drop` de cada flujo cierra primero y libera su estado después).
- **Contrato con Kotlin** (la app): antes de que Rust llame a `start`, Kotlin **tiene el permiso
  `RECORD_AUDIO`** (sin él, `start` falla al abrir la entrada), **pone
  `AudioManager.mode = MODE_IN_COMMUNICATION`** (y la ruta: auricular, altavoz, cascos) y, para
  seguir en segundo plano, mantiene el servicio en primer plano de tipo `microphone`. El modo se
  restaura después de `stop`. Rust no toca el framework de Android.

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

### Vídeo en iOS (`video::ios`)

Solo `target_os = "ios"`; las partes puras (`h264`, `orientation`, `settings`, `keyframes`) se
compilan también en los tests del host (`cfg(any(target_os = "ios", test))`).

- **`CameraSource`** (`VideoSource`): `AVCaptureSession` con la cámara gran angular integrada
  (frontal o trasera), preset más pequeño que cubre `VideoConfig` (352×288, 640×480, 1280×720,
  1920×1080) → `AVCaptureVideoDataOutput` en NV12 (`420v`), descartando tramas tardías, a un
  delegado (clase Objective-C definida con objc2) en una cola serie de GCD → ritmo a `fps`
  (`FramePacer`) → `VTCompressionSession` creada en la primera trama con el tamaño real (y otra
  vez si cambia al cambiar de cámara) → Annex-B con SPS/PPS delante de cada IDR → `FrameSender`.
  `switch_camera` cambia la entrada en caliente y fuerza keyframe. `preview_layer()` devuelve el
  `AVCaptureVideoPreviewLayer`. `set_device_orientation(DeviceOrientation)` y `encode_errors()`.
- **Codificador**: Constrained Baseline (`kVTProfileLevel_H264_ConstrainedBaseline_AutoLevel`;
  si no está, Baseline), `RealTime`, `AllowFrameReordering = false` (sin B-frames),
  `MaxKeyFrameInterval` = 2 s en tramas y `MaxKeyFrameIntervalDuration` = 2 s, `ExpectedFrameRate`,
  `AverageBitRate` = bitrate acotado y `DataRateLimits` = [1,5 × bitrate / 8 bytes, 1 s] (como
  libwebrtc). `request_keyframe`, `FrameSender::keyframe_needed` y un codificador nuevo fuerzan
  keyframe con `kVTEncodeFrameOptionKey_ForceKeyFrame`. `set_bitrate` se aplica en la trama
  siguiente sin reiniciar. En el simulador sale `42 e0 1e` (Constrained Baseline, nivel 3.0).
- **Rotación**: no se rota ni se espeja el píxel; la tabla de libwebrtc (`RTCCameraVideoCapturer`):
  vertical → 90°, boca abajo → 270°, apaisado izquierda → 0° trasera / 180° frontal, apaisado
  derecha → 180° trasera / 0° frontal; boca arriba/abajo o desconocida mantiene la última.
- **`DisplaySink`** (`VideoSink`): Annex-B → `CMVideoFormatDescription` desde SPS/PPS (se rehace
  si cambian) → `CMSampleBuffer` AVCC con `DisplayImmediately` (el ritmo lo pone el jitter buffer)
  → `enqueueSampleBuffer:` de un **`AVSampleBufferDisplayLayer`**, que decodifica por hardware.
  La `Rotation` se aplica como `affineTransform` de la capa dentro de un `CATransaction` explícito
  sin animación. `layer()`, `rotation()`, `keyframe_requests()`.
- **Petición de keyframe del lado que pinta** (`KeyframeRequests`): un indicador compartido. El
  `DecodeGate` descarta las tramas delta mientras no haya referencia (al empezar, tras `stop`, tras
  un fallo de la capa —`status == .failed` o `requiresFlushToResumeDecoding`, que se resuelve con
  `flush`—, tras una trama que la capa no pudo aceptar o que no era H.264) y pide keyframe como
  mucho cada 500 ms de tiempo de trama. **El pipeline sondea `take()`** (tras cada `push` o con un
  temporizador) y, si da `true`, manda un RTCP PLI; las peticiones entre dos sondeos se juntan en
  una. `total()` cuenta todas.
- **Contrato con Swift**:
  - **Capas**: `preview_layer()` y `layer()` son `CALayer` que posee el objeto de Rust, válidas
    hasta que se suelta. Swift las toma con `Unmanaged<CALayer>.fromOpaque(ptr)
    .takeUnretainedValue()`, las añade como subcapas **en el hilo principal**, las coloca en
    `layoutSubviews` y las quita antes de soltar el objeto. Con rotación de 90°/270° la capa
    remota se coloca con `bounds` (ancho y alto cambiados) y `position`, nunca con `frame`.
  - **Permiso**: la app declara `NSCameraUsageDescription` y pide acceso
    (`AVCaptureDevice.requestAccess(for: .video)`) antes de la videollamada. El motor no lo pide:
    `start` devuelve `PermissionDenied` si no está concedido (sin preguntar aún también cuenta
    como denegado; es lo que da el simulador).
  - **Orientación**: UIKit es solo del hilo principal, así que la app llama a
    `set_device_orientation(DeviceOrientation::from_raw(UIDevice.current.orientation.rawValue))`
    desde su observador de `orientationDidChangeNotification`. Empieza en vertical.
  - **`AVAudioSession`**: no se toca; es de la app y de CallKit.
- **FFI**: VideoToolbox, CoreMedia, CoreVideo, CoreFoundation y libdispatch son C y se declaran a
  mano en `video/ios/ffi.rs` (con `const` que fijan el tamaño de `CMTime`, `CMSampleTimingInfo`,
  `CGAffineTransform`…). AVFoundation y Core Animation van por **`objc2` solo** (dependencia solo
  para iOS): define la clase del delegado, lleva los retain/release y comprueba en debug la
  codificación de tipos de cada mensaje. Los crates generados (`objc2-av-foundation` y familia)
  traerían media docena de crates y decenas de features para unos treinta mensajes.
- **Tiempo real**: el callback de captura no espera nunca (`try_lock` del estado; el hilo del
  motor solo lo coge con la cola drenada), el de VideoToolbox copia y hace `try_send`. Crear el
  codificador (primera trama, cambio de tamaño) sí ocurre en la cola de captura.
- **Parada**: `stopRunning`, quitar el delegado, drenar la cola (`dispatch_sync_f`) y soltar el
  estado de captura; soltar el codificador espera sus tramas pendientes y lo invalida antes de
  liberar su salida.

### Vídeo en Android (`video::android`)

Todo el detalle está en la doc del módulo (`src/video/android.rs`). En corto:

- **Captura**: Camera2 del NDK escribe directamente en la **superficie de entrada del codificador**
  (`AMediaCodec_createInputSurface`): cero copias. H.264 Baseline nivel 3.1, CBR (si el
  codificador no lo acepta, VBR; si tampoco, sin perfil ni nivel), keyframe cada 2 s, prioridad
  de tiempo real. Un hilo de drenaje guarda SPS/PPS (buffer de codec-config) y los pone delante de
  **cada** keyframe; MediaCodec ya da Annex-B (comprobado en la tableta; si diera AVCC se
  convierte). `set_bitrate` → `video-bitrate`, `request_keyframe` → `request-sync`
  (`AMediaCodec_setParameters`); el hilo también pide keyframe cuando el canal lo necesita (una
  vez, y otra si en 30 tramas no llega). La rotación sale de la orientación del sensor, la cámara
  (frontal: sensor + pantalla; trasera: sensor − pantalla) y la rotación de la pantalla; no se
  rota ni se espeja el píxel. El tamaño se elige entre los que comparten las dos cámaras, así el
  cambio de cámara conserva el codificador (y pide keyframe).
- **Reproducción**: el decodificador se configura con el primer keyframe (`csd-0`/`csd-1` y el
  tamaño leído del SPS, con recorte) y pinta cada trama en la superficie en cuanto sale
  (`releaseOutputBuffer(render = true)`), desde su propio hilo. **La rotación la aplica el
  decodificador** (`rotation-degrees`): un `SurfaceView` normal la muestra derecha sin trabajo en
  Kotlin. Cambiarla exige otro decodificador: se espera al siguiente keyframe (mientras, se ve con
  la rotación anterior y se pide keyframe). `DisplaySink::video_size()` da el tamaño ya girado
  para el aspecto de la vista.
- **Señal de keyframe**: `DisplaySink::keyframe_request()` devuelve un `KeyframeRequest` que la
  cadena sondea; mientras `is_needed()` sea `true`, manda PLI (con su propio límite de ritmo). Se
  enciende antes del primer keyframe, al perder una trama (sin búfer de entrada libre), tras un
  error del decodificador (se reconstruye en el siguiente keyframe), tras un cambio de superficie
  que no pudo seguir y mientras espera un cambio de rotación. Los errores del decodificador nunca
  hacen fallar `push`.
- **Carga**: `libmediandk.so` y `libcamera2ndk.so` con `dlopen` (como AAudio), para que la
  biblioteca cargue en API 24. `libandroid.so` se enlaza (existe siempre).
- **Versiones**: `CameraSource` exige **Android 8.0 (API 26)** (superficie de entrada y
  `setParameters`); antes, `start` devuelve `VideoError::Unsupported`. No se hizo el camino con
  copia (`AImageReader` YUV → búferes de entrada): en API 24–25 tampoco habría control de bitrate
  ni keyframe a demanda. `DisplaySink` funciona desde API 24; desde API 26, sin superficie de la
  app el decodificador pinta en un `AImageReader` de relleno y cambia de superficie sin reiniciar
  (`setOutputSurface`); en 24–25 un cambio de superficie lo reconstruye en el siguiente keyframe.
- **Tiempo real**: nada de codec en callbacks de la plataforma: hilo propio por codec con espera
  de 10 ms (los callbacks asíncronos son API 28). Los callbacks de la cámara solo tocan atómicos o
  avisan de que la sesión se cerró.

**Contrato con Kotlin**:

1. Tener el permiso `CAMERA` antes de `CameraSource::start`; sin él (o con la cámara desactivada
   por política) devuelve `VideoError::PermissionDenied`.
2. Pasar cada `Surface` por JNI; el lado Rust de la app hace `ANativeWindow_fromSurface` y llama a
   `CameraSource::set_preview_surface` (vista previa local) o `DisplaySink::set_surface` (vídeo
   remoto). Ambos toman su propia referencia: JNI puede hacer `ANativeWindow_release` de la suya
   enseguida. En `surfaceDestroyed`, pasar `None` antes de volver.
3. Usar `SurfaceView`: la vista previa recibe la transformación de la cámara (derecha y espejada en
   la frontal) y el vídeo remoto, la rotación del decodificador.
4. Llamar a `CameraSource::set_display_rotation` al girar la actividad (no hace falta en una
   pantalla solo vertical) y reiniciar la fuente si `camera_lost()` (otra app cogió la cámara).
5. Para vídeo en segundo plano, servicio en primer plano de tipo `camera`.

Notas: el proceso necesita hilos de binder para que la cámara rellene superficies cuya cola vive
en él (la app siempre los tiene; en un ejecutable de `adb shell` no, y la cámara se bloquea al
pedir el primer búfer: las pruebas los arrancan con `ABinderProcess_startThreadPool`). El
decodificador MediaTek de la tableta escribe en formato por bloques: leer un `AImageReader` YUV
desde la CPU lo ve «rayado»; la prueba compara proporciones de claros y oscuros, no posiciones.

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
cargo test --test loopback -- --nocapture          # cadena completa sobre webrtc-rs en loopback (audio y vídeo)
cargo test --test video_pipeline -- --nocapture    # vídeo sobre el simulador (red limpia y con pérdida)
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
IPHONEOS_DEPLOYMENT_TARGET=15.0 cargo build --target aarch64-apple-ios-sim
IPHONEOS_DEPLOYMENT_TARGET=15.0 cargo clippy --all-targets --target aarch64-apple-ios -- -D warnings

# Tests del vídeo de iOS en el simulador (uno arrancado: xcrun simctl list devices booted). Sin
# cámara: los #[ignore] codifican tramas NV12 sintéticas con VideoToolbox, las decodifican con
# VTDecompressionSession y con DisplaySink, y llaman al delegado de captura con CMSampleBuffers
# sintéticos.
IPHONEOS_DEPLOYMENT_TARGET=15.0 cargo test --target aarch64-apple-ios-sim --lib --no-run
xcrun simctl spawn <id> target/aarch64-apple-ios-sim/debug/deps/webrtc_engine-<hash> video::ios
xcrun simctl spawn <id> target/aarch64-apple-ios-sim/debug/deps/webrtc_engine-<hash> \
  --ignored --test-threads 1 video::ios

# Tests en el simulador de iOS (con uno arrancado: xcrun simctl list devices booted). El binario
# se lanza con simctl spawn, sin instalar ninguna app; los #[ignore] usan la unidad de voz real
# con el micrófono del Mac.
IPHONEOS_DEPLOYMENT_TARGET=15.0 cargo test --target aarch64-apple-ios-sim --no-run
xcrun simctl spawn booted target/aarch64-apple-ios-sim/debug/deps/webrtc_engine-<hash>
xcrun simctl spawn booted target/aarch64-apple-ios-sim/debug/deps/webrtc_engine-<hash> \
  --ignored --test-threads 1 audio::ios

# Android (mismas variables del NDK para build, clippy y tests)
NDK_BIN=~/Library/Android/sdk/ndk/27.1.12297006/toolchains/llvm/prebuilt/darwin-x86_64/bin
CC_aarch64_linux_android=$NDK_BIN/aarch64-linux-android24-clang \
AR_aarch64_linux_android=$NDK_BIN/llvm-ar \
CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER=$NDK_BIN/aarch64-linux-android24-clang \
cargo build --target aarch64-linux-android
cargo clippy --target aarch64-linux-android --all-targets -- -D warnings

# Pruebas de audio en un Android por adb (solo si Ioan lo pide; se borra el binario al acabar).
# Desde el usuario shell se abren también la entrada (el shell tiene RECORD_AUDIO).
cargo test --target aarch64-linux-android --lib --no-run   # imprime la ruta del binario
adb push target/aarch64-linux-android/debug/deps/webrtc_engine-<hash> /data/local/tmp/wee-tests
adb shell "cd /data/local/tmp && ./wee-tests audio::android --include-ignored --test-threads=1 --nocapture"
adb shell rm /data/local/tmp/wee-tests

# Vídeo Android en la tableta (mismas variables): el binario de tests, a /data/local/tmp y fuera
cargo test --target aarch64-linux-android --lib --no-run      # imprime la ruta del ejecutable
adb push <ejecutable> /data/local/tmp/wee-video-test
adb shell 'cd /data/local/tmp && ./wee-video-test --ignored video::android --test-threads=1 --nocapture'
adb shell rm /data/local/tmp/wee-video-test
```

La prueba de ventana (`shows_the_test_pattern_in_a_window`, `#[ignore]`) no puede pasar en macOS
con libtest, que no ejecuta en el hilo principal: la ventana se comprueba con `video_demo`.

Las pruebas de `build.rs` no las ejecuta Cargo; el comando está en el propio `build.rs`.

CI (`.github/workflows/ci.yml`): en macOS fmt, clippy con y sin `desktop`, tests, y clippy y
build de iOS; en Ubuntu, build y clippy de Android con el NDK del runner (sin `desktop`, que
pediría las cabeceras de ALSA). El código solo de iOS o de Android solo lo revisa el clippy de su
target. Sin secretos.

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

- **Backend iOS en la app**: el lado Swift (sesión de audio, CallKit, reinicio tras una
  interrupción con anillos nuevos) y probarlo en el iPhone con la pantalla bloqueada.
- **Android, integración**: llamar a `maintain()` cada ~100 ms desde quien tenga el backend
  (en la app, junto a la `Call`); probar una desconexión real (enchufar cascos o Bluetooth en
  plena llamada), Android 8.x (sin preset) y más teléfonos; comprobar el AEC en una llamada real.
- **Vídeo Android**: probar `CameraSource`/`DisplaySink` dentro de la app (JNI con las
  `Surface`, permiso, servicio en primer plano) y en más fabricantes; decidir si se marca el SPS
  como Constrained Baseline (la tableta da `42 00 1f`: Baseline sin `constraint_set1`, aunque la
  SDP anuncie `42e01f`) o se pide `AVCProfileConstrainedBaseline` (API 27) cuando exista; camino
  con copia para API 24–25 si hiciera falta.
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
- **Vídeo iOS**: probarlo con la cámara real del iPhone (en el simulador no hay cámara) y con la
  pantalla bloqueada/en segundo plano (la cámara se para; la capa se vacía y debe pedir keyframe).
  El delegado no se declara conforme al protocolo `AVCaptureVideoDataOutputSampleBufferDelegate`
  (AVFoundation solo mira `respondsToSelector:`); confirmarlo en el dispositivo. La frecuencia de
  la cámara no se fija (`activeVideoMinFrameDuration`): se diezman tramas. En iOS 17+ los métodos de
  encolar de la capa pasan a `sampleBufferRenderer`; con objetivo 15 se usan los de la capa.
  Contador de fallos de la capa y de tramas descartadas por el `DecodeGate` para las estadísticas.
- **`video_demo` sobre `VideoCall`**: hoy usa su propio enlace (`FrameLink`: una trama entera por
  paquete simulado, reordenación con 80 ms de espera, pérdida → keyframe). Cuando se fusione
  `VideoCall` (`VideoSender`/`VideoReceiver`), pasarla a `VideoCall` sobre `simulated_link`
  (hay un `TODO` en la demo).
- nokhwa 0.10 arrastra `block` 0.1.6, que Rust avisa que dejará de compilar
  (`future-incompat`); si llega a romper, sustituir nokhwa por `objc2-av-foundation`.
- La demo de vídeo aún no la ha visto nadie con la cámara: falta que Ioan la ejecute.

## Estado (2026-09-28)

**Backends de los teléfonos** (rama `backends`, desde `audio-engine`): fusionadas
`ios-backend` y `android-backend` (conflictos solo en `README.md` y `CLAUDE.md`, resueltos
conservando los dos lados; `ci.yml`, `Cargo.toml`, `Cargo.lock` y `src/audio.rs` sin conflicto).
Encima:

- **`AudioBackend::maintain()`**, método por defecto que no hace nada: se llama cada ~100 ms
  mientras el backend funciona, desde el hilo que lo tiene y nunca desde un callback. Android lo
  implementa con `restart_if_needed()`, así el deber de reabrir los flujos tras una desconexión
  está en el trait y no se olvida; iOS y el escritorio usan el de por defecto.
- **`audio::platform_backend() -> Result<Box<dyn AudioBackend + Send>, AudioError>`**: el
  backend de la plataforma, parado (`ios::VoiceProcessingBackend::new()`,
  `android::AaudioBackend::new()`, `desktop::DesktopBackend::new()` con la feature `desktop`, o
  `AudioError::NoDevice`). Los nombres de cada backend no cambian: la app programa contra ellos.
- Tests: el `maintain` por defecto y su paso por un `Box<dyn AudioBackend + Send>` con backends
  falsos, `platform_backend` en el host con y sin `desktop`, en el simulador de iOS (pasa) y en
  Android (solo compilado; sin ejecutar en un dispositivo).

**Backend iOS** (rama `ios-backend`): `audio::ios::VoiceProcessingBackend` sobre
`VoiceProcessingIO`, con FFI a mano. 23 tests en el host; en el simulador de iOS pasan todos los
tests del crate y los dos `#[ignore]` de la unidad real (arranca, captura ~50 tramas por segundo,
vacía la cola del altavoz, para y vuelve a arrancar). Sin probar aún en un iPhone.

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
- `audio::android` (rama `android-backend`): `AaudioBackend` sobre AAudio cargado con `dlopen`.
  Probado en la tableta Lenovo TB361FU (Android 16, API 36): salida y entrada concedidas a
  48 kHz mono i16, AudioFlinger crea el AEC y la supresión de ruido en la sesión compartida, la
  reapertura tras un error funciona y la batería completa de tests de la biblioteca pasa en el
  dispositivo.
- CI en `.github/workflows/ci.yml`.
- Vídeo (rama `video-core`): núcleo común sobre el contrato `video` — empaquetado y reensamblado
  H.264 (FU-A, STAP-A), keyframes y PLI, control de bitrate, `VideoCall` con pausa, cambio de
  cámara y estadísticas, `FakeSource`/`FakeSink`, enlace de vídeo del simulador y pistas H.264 en
  webrtc-rs con CVO e interceptor de feedback. Pruebas: sobre el simulador (limpia: todas las
  tramas en orden, sin peticiones; 1 % de pérdida y reordenación: se piden keyframes, llegan a la
  cámara y la pantalla nunca muestra basura) y en loopback real (Opus y H.264 en la misma
  conexión, rotación por CVO, PLI hasta la cámara).
- Rama `video-ios`: `video::ios` (`CameraSource`, `DisplaySink`, `KeyframeRequests`,
  `DeviceOrientation`). En el simulador de iOS 27 pasan los 11 tests `#[ignore]`: 30 tramas
  sintéticas → Annex-B con SPS/PPS en el primer keyframe → 30 imágenes 640×480 decodificadas por
  `VTDecompressionSession`, y la misma secuencia en `DisplaySink` sin pedir keyframes con la capa
  en `.rendering`; keyframe forzado y cambio de bitrate en vivo; delegado de captura con ritmo,
  rotación y cambio de tamaño; `start` sin permiso → `PermissionDenied`. Sin probar aún en el
  iPhone.
- Vídeo Android (rama `video-android`): `video::android::{CameraSource, DisplaySink}` con 43
  pruebas en el host y 5 en la tableta Lenovo (API 36, MediaTek; 3 de ellas `#[ignore]`): codificación desde la
  superficie de entrada → Annex-B con SPS/PPS en cada keyframe (el keyframe pedido llega, 640×480,
  el flujo lo decodifica también ffmpeg) → `DisplaySink` sobre un `AImageReader` (79 imágenes de
  90 tramas, cambio de superficie sin reinicio); cámara frontal con vista previa, cambio a la
  trasera (rotaciones 270 y 90) y fuera la vista previa.
- Vídeo por software y de escritorio (rama `video-desktop`): `video::openh264` (codificador y
  decodificador OpenH264; VGA a 800 kbit/s con la imagen de prueba: PSNR de luma ≥ 34 dB en las
  30 primeras tramas; 150 kbit/s → ~149 kbit/s medidos, 2 Mbit/s → ~660 kbit/s, sin keyframe al
  cambiar; una trama perdida da error y luego nada hasta el keyframe, que recupera ≥ 30 dB),
  `video::desktop` (cámara, sumidero y ventana) y la demo `video_demo`, que compila pero falta
  ver con la cámara.
