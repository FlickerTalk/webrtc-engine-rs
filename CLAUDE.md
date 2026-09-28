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
| `video`  | Contrato del vídeo: la plataforma captura **y** codifica (y decodifica **y** pinta) con su hardware; el núcleo solo mueve access units H.264 Annex-B (`EncodedFrame`, SPS/PPS en cada keyframe). `VideoSource`, `VideoSink`, `VideoConfig`, `Facing`, `Rotation` (viaja en la extensión RTP CVO, no se rota el píxel) y el canal `frame_channel` (acotado, sin bloqueo; tras descartar exige keyframe). Puntos de anclaje de iOS/Android/escritorio en la doc del módulo. `video::android`: `CameraSource` y `DisplaySink` (abajo). |

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

# iOS
IPHONEOS_DEPLOYMENT_TARGET=15.0 cargo build --target aarch64-apple-ios

# Android
NDK_BIN=~/Library/Android/sdk/ndk/27.1.12297006/toolchains/llvm/prebuilt/darwin-x86_64/bin
CC_aarch64_linux_android=$NDK_BIN/aarch64-linux-android24-clang \
AR_aarch64_linux_android=$NDK_BIN/llvm-ar \
CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER=$NDK_BIN/aarch64-linux-android24-clang \
cargo build --target aarch64-linux-android

# Vídeo Android en la tableta (mismas variables): el binario de tests, a /data/local/tmp y fuera
cargo test --target aarch64-linux-android --lib --no-run      # imprime la ruta del ejecutable
adb push <ejecutable> /data/local/tmp/wee-video-test
adb shell 'cd /data/local/tmp && ./wee-video-test --ignored video::android --test-threads=1 --nocapture'
adb shell rm /data/local/tmp/wee-video-test
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
- Vídeo Android (rama `video-android`): `video::android::{CameraSource, DisplaySink}` con 43
  pruebas en el host y 5 en la tableta Lenovo (API 36, MediaTek; 3 de ellas `#[ignore]`): codificación desde la
  superficie de entrada → Annex-B con SPS/PPS en cada keyframe (el keyframe pedido llega, 640×480,
  el flujo lo decodifica también ffmpeg) → `DisplaySink` sobre un `AImageReader` (79 imágenes de
  90 tramas, cambio de superficie sin reinicio); cámara frontal con vista previa, cambio a la
  trasera (rotaciones 270 y 90) y fuera la vista previa.
