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
| `audio`  | Anillos SPSC sin bloqueos ni asignaciones entre los callbacks del dispositivo y el motor; adaptadores (mezcla a mono, remuestreo lineal, i16/f32, silencio y cuenta de vacíos); trait `AudioBackend`; `audio::desktop` con cpal (feature `desktop`, desactivada por defecto); `audio::ios` con `VoiceProcessingBackend` (solo iOS); `audio::android` con `AaudioBackend` sobre AAudio (solo Android). |
| `codec`  | Opus con libopus 1.6.1 vendorizada en `vendor/opus` y compilada con `cc` (`build.rs`, sin cmake). VoIP, 32 kbit/s, FEC en banda, 10 % de pérdida esperada, DTX apagado. `decode`, `conceal` (PLC), `recover` (FEC del paquete siguiente). |
| `rtp`    | Opus sobre pistas de webrtc-rs 0.21 (API sans-IO + crate `rtc`): `media_engine`, `peer_connection_builder`, `add_audio_track` → `AudioSender`, `AudioReceiver`. El payload type negociado se lee en cada envío. |
| `jitter` | `JitterBuffer`: reordena por secuencia (con el salto de 65535 a 0), profundidad adaptativa de 1 a 10 tramas según el jitter RFC 3550 medido con `push_at`; `playout()` → `Frame` / `Missing` / `Waiting`; `peek_next` para la FEC. |
| `call`   | La cadena. `Uplink` y `Downlink` son máquinas de estado síncronas; `Call` las ejecuta en tres tareas de Tokio (envío, recepción, reproducción). Transporte abstracto: `PacketSink` / `PacketSource`, implementados para `AudioSender`, `AudioReceiver` y `RemoteAudio`. |
| `netsim` | `NetworkSimulator`: red simulada sans-IO y determinista con semilla (retardo, jitter uniforme, pérdida, reordenación, duplicados). `simulated_link` la usa como transporte de una `Call` con el reloj de Tokio. |

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
  `restart_if_needed()` cada ~100 ms**: cierra los dos flujos y los reabre sobre los mismos
  anillos; si falla, se queda con los anillos y lo reintenta en la siguiente llamada.
- **Parada**: `stop` y `Drop` paran y cierran los dos flujos antes de soltar los adaptadores y
  los anillos (el `Drop` de cada flujo cierra primero y libera su estado después).
- **Contrato con Kotlin** (la app): antes de que Rust llame a `start`, Kotlin **tiene el permiso
  `RECORD_AUDIO`** (sin él, `start` falla al abrir la entrada), **pone
  `AudioManager.mode = MODE_IN_COMMUNICATION`** (y la ruta: auricular, altavoz, cascos) y, para
  seguir en segundo plano, mantiene el servicio en primer plano de tipo `microphone`. El modo se
  restaura después de `stop`. Rust no toca el framework de Android.

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
IPHONEOS_DEPLOYMENT_TARGET=15.0 cargo build --target aarch64-apple-ios-sim
IPHONEOS_DEPLOYMENT_TARGET=15.0 cargo clippy --all-targets --target aarch64-apple-ios -- -D warnings

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

- **Backend iOS en la app**: el lado Swift (sesión de audio, CallKit, reinicio tras una
  interrupción con anillos nuevos) y probarlo en el iPhone con la pantalla bloqueada.
- **Android, integración**: llamar a `restart_if_needed()` periódicamente desde quien tenga el
  backend; probar una desconexión real (enchufar cascos o Bluetooth en plena llamada), Android
  8.x (sin preset) y más teléfonos; comprobar el AEC en una llamada real.
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
