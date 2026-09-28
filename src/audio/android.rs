//! Android backend on AAudio: [`AaudioBackend`] (Android only).
//!
//! # Streams
//!
//! Two shared, low-latency streams, asking for 48 kHz mono i16; whatever AAudio grants is read
//! back and converted by the adapters (i16 or float, any rate or channel count).
//!
//! - Input: preset `VOICE_COMMUNICATION`, which brings the platform's echo canceller and noise
//!   suppressor.
//! - Output: usage `VOICE_COMMUNICATION`, content type speech, buffer of two bursts.
//! - The input opens first with a new audio session (`AAUDIO_SESSION_ID_ALLOCATE`) and the output
//!   joins that session, so the echo canceller pairs them. A session id rules out MMAP, so
//!   latency is a little higher; echo cancellation is worth it.
//!
//! # Android versions
//!
//! AAudio is looked up at run time (`dlopen`), not linked: the app's minimum is Android 7
//! (API 24), where it does not exist, and a linked `libaaudio.so` would stop the whole native
//! library from loading there. On API 24 and 25, [`AaudioBackend::new`] returns an
//! [`AudioError`]. On API 26 and 27, AAudio has no presets, usage or sessions: the call works
//! without platform voice processing, and [`AaudioBackend::voice_processing`] says so.
//!
//! # Real time and errors
//!
//! The data callbacks only move samples between AAudio's buffer and the lock-free rings through
//! the adapters: no locks, no allocation, no logging. The error callback (a device disconnected,
//! a headset plugged in or out) only counts the error and raises a flag, since AAudio forbids
//! closing a stream from it. The owner polls [`AaudioBackend::restart_if_needed`], which closes
//! both streams and reopens them on the same rings.
//!
//! # The app's side (Kotlin) before `start`
//!
//! The backend does not touch the Android framework. Before Rust calls
//! [`AudioBackend::start`](super::AudioBackend::start), the app's Kotlin code must:
//!
//! 1. hold the `RECORD_AUDIO` permission (without it the input stream fails to open and
//!    `start` returns an error);
//! 2. set `AudioManager.mode = MODE_IN_COMMUNICATION` (and pick the route: earpiece, speaker,
//!    headset), and keep it until after [`AudioBackend::stop`](super::AudioBackend::stop);
//! 3. for a call in the background, run the foreground service of type `microphone` (and
//!    `phoneCall` if it uses the telecom framework) that keeps the process and the microphone
//!    alive.
//!
//! [`AudioBackend::stop`](super::AudioBackend::stop) and `Drop` stop and close both streams before
//! the adapters and the rings go; after `stop` the app can restore the audio mode.

use std::ffi::c_void;
use std::slice;
use std::sync::atomic::{AtomicBool, Ordering};

use super::format::StreamFormat;
use super::{AudioError, CaptureAdapter, Counter, PlayoutAdapter};

/// `aaudio_format_t` values (`<aaudio/AAudio.h>`).
const FORMAT_PCM_I16: i32 = 1;
const FORMAT_PCM_FLOAT: i32 = 2;
/// `aaudio_data_callback_result_t`: keep calling.
const CALLBACK_RESULT_CONTINUE: i32 = 0;

/// `AAudioStream`, opaque.
#[repr(C)]
struct AAudioStream {
    _private: [u8; 0],
}

/// The sample type of an open stream's buffers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SampleKind {
    I16,
    F32,
}

/// The format AAudio actually granted, as the adapters need it. The engine asks for 48 kHz mono
/// i16, but AAudio may open something else; the adapters convert whatever it is.
fn granted_format(
    sample_rate: i32,
    channels: i32,
    format: i32,
) -> Result<(StreamFormat, SampleKind), AudioError> {
    let invalid = || AudioError::InvalidFormat {
        sample_rate: u32::try_from(sample_rate).unwrap_or(0),
        channels: u16::try_from(channels).unwrap_or(0),
    };
    let rate = u32::try_from(sample_rate)
        .ok()
        .filter(|&rate| rate > 0)
        .ok_or_else(invalid)?;
    let channels = u16::try_from(channels)
        .ok()
        .filter(|&channels| channels > 0)
        .ok_or_else(invalid)?;
    let kind = match format {
        FORMAT_PCM_I16 => SampleKind::I16,
        FORMAT_PCM_FLOAT => SampleKind::F32,
        other => {
            return Err(AudioError::Backend(format!(
                "AAudio opened sample format {other}, not i16 or float"
            )));
        }
    };
    let format = StreamFormat {
        sample_rate: rate,
        channels,
    };
    Ok((format, kind))
}

/// The playout buffer to ask for: two bursts, AAudio's advice for low latency without glitches,
/// within the stream's capacity. `None` leaves AAudio's default.
fn playout_buffer_frames(frames_per_burst: i32, capacity: i32) -> Option<i32> {
    if frames_per_burst <= 0 {
        return None;
    }
    let wanted = frames_per_burst.saturating_mul(2);
    Some(if capacity > 0 {
        wanted.min(capacity)
    } else {
        wanted
    })
}

/// What a stream's data callback does with each buffer, in the format the stream opened with.
enum Processor {
    Capture {
        adapter: CaptureAdapter,
        kind: SampleKind,
        channels: usize,
    },
    Playout {
        adapter: PlayoutAdapter,
        kind: SampleKind,
        channels: usize,
    },
}

impl Processor {
    /// Hands one callback buffer to the adapter.
    ///
    /// # Safety
    ///
    /// `audio` is null or points to `frames` frames of interleaved samples of this processor's
    /// kind and channel count, valid and not aliased for the duration of the call.
    unsafe fn process(&mut self, audio: *mut c_void, frames: i32) {
        let Ok(frames) = usize::try_from(frames) else {
            return;
        };
        if audio.is_null() || frames == 0 {
            return;
        }
        // SAFETY (all four): the caller guarantees `frames` frames of this processor's kind and
        // channel count behind `audio`, used by nobody else during the call.
        match self {
            Self::Capture {
                adapter,
                kind: SampleKind::I16,
                channels,
            } => adapter.push::<i16>(unsafe { buffer(audio, frames, *channels) }),
            Self::Capture {
                adapter,
                kind: SampleKind::F32,
                channels,
            } => adapter.push::<f32>(unsafe { buffer(audio, frames, *channels) }),
            Self::Playout {
                adapter,
                kind: SampleKind::I16,
                channels,
            } => adapter.fill::<i16>(unsafe { buffer(audio, frames, *channels) }),
            Self::Playout {
                adapter,
                kind: SampleKind::F32,
                channels,
            } => adapter.fill::<f32>(unsafe { buffer(audio, frames, *channels) }),
        }
    }
}

/// A callback's buffer as a slice of samples.
///
/// # Safety
///
/// `audio` points to `frames * channels` initialised samples of type `S`, valid and used by
/// nobody else for `'a`.
unsafe fn buffer<'a, S>(audio: *mut c_void, frames: usize, channels: usize) -> &'a mut [S] {
    // SAFETY: the caller's guarantee.
    unsafe { slice::from_raw_parts_mut(audio.cast::<S>(), frames.saturating_mul(channels)) }
}

/// The data callback's state. AAudio holds a raw pointer to it from opening the stream until
/// closing it; it stays empty until the stream's format is known and it gets a [`Processor`].
#[derive(Default)]
struct CallbackSlot(Option<Processor>);

/// What the error callbacks of both streams report to the backend. Atomics only: the error
/// callback runs on an AAudio thread and must not block.
#[derive(Default)]
struct StreamHealth {
    errors: Counter,
    restart_needed: AtomicBool,
}

impl StreamHealth {
    /// Notes an error: AAudio only reports errors that end the stream, such as a disconnect.
    fn report(&self) {
        self.errors.add(1);
        self.restart_needed.store(true, Ordering::Release);
    }

    /// Asks for a restart again, after one failed.
    #[cfg(target_os = "android")]
    fn request_restart(&self) {
        self.restart_needed.store(true, Ordering::Release);
    }

    /// Whether a stream died since the last call, clearing the flag.
    fn take_restart(&self) -> bool {
        self.restart_needed.swap(false, Ordering::AcqRel)
    }
}

/// `AAudioStream_dataCallback`. Runs on AAudio's real-time thread: no locks, no allocation.
///
/// # Safety
///
/// `user_data` is null or the [`CallbackSlot`] registered with this stream, used by nobody
/// else while the stream runs; `audio` holds `frames` frames in the stream's format.
unsafe extern "C" fn on_data(
    _stream: *mut AAudioStream,
    user_data: *mut c_void,
    audio: *mut c_void,
    frames: i32,
) -> i32 {
    // SAFETY: the caller guarantees `user_data` is null or our slot, borrowed by nobody else.
    if let Some(CallbackSlot(Some(processor))) =
        unsafe { user_data.cast::<CallbackSlot>().as_mut() }
    {
        // SAFETY: AAudio hands over `frames` frames in the format the processor was made for.
        unsafe { processor.process(audio, frames) };
    }
    CALLBACK_RESULT_CONTINUE
}

/// `AAudioStream_errorCallback`. Must not stop or close the stream (AAudio forbids it from this
/// thread): it only raises the flag the owner polls.
///
/// # Safety
///
/// `user_data` is null or points to a [`StreamHealth`] that outlives the stream.
unsafe extern "C" fn on_error(_stream: *mut AAudioStream, user_data: *mut c_void, _error: i32) {
    // SAFETY: the caller guarantees `user_data` is null or a live `StreamHealth`; it is only
    // read through a shared reference, and it only holds atomics.
    if let Some(health) = unsafe { user_data.cast::<StreamHealth>().as_ref() } {
        health.report();
    }
}

#[cfg(target_os = "android")]
pub use device::AaudioBackend;

#[cfg(target_os = "android")]
mod device {
    use std::ffi::{CStr, c_char};
    use std::ptr::{self, NonNull};
    use std::sync::{Arc, OnceLock};

    use super::*;
    use crate::SAMPLE_RATE;
    use crate::audio::{AudioBackend, DeviceIo};

    // Constants from `<aaudio/AAudio.h>`.
    const OK: i32 = 0;
    const SHARING_MODE_SHARED: i32 = 1;
    const PERFORMANCE_MODE_LOW_LATENCY: i32 = 12;
    const USAGE_VOICE_COMMUNICATION: i32 = 2;
    const CONTENT_TYPE_SPEECH: i32 = 1;
    const INPUT_PRESET_VOICE_COMMUNICATION: i32 = 7;
    /// `AAUDIO_SESSION_ID_ALLOCATE`: a new session for the effects to attach to.
    pub(super) const SESSION_ID_ALLOCATE: i32 = 0;

    /// A stream direction (`aaudio_direction_t`).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) enum Direction {
        Output = 0,
        Input = 1,
    }

    /// `AAudioStreamBuilder`, opaque.
    #[repr(C)]
    struct AAudioStreamBuilder {
        _private: [u8; 0],
    }

    type DataCallback =
        unsafe extern "C" fn(*mut AAudioStream, *mut c_void, *mut c_void, i32) -> i32;
    type ErrorCallback = unsafe extern "C" fn(*mut AAudioStream, *mut c_void, i32);
    type BuilderSetter = unsafe extern "C" fn(*mut AAudioStreamBuilder, i32);
    type StreamCall = unsafe extern "C" fn(*mut AAudioStream) -> i32;

    /// The AAudio functions, looked up at run time.
    ///
    /// Why not link `libaaudio.so`: the app's minimum is Android 7 (API 24), which has no
    /// AAudio, and a missing `DT_NEEDED` library would stop the whole native library from
    /// loading there. Looking it up turns that into an `AudioError` from `new`.
    struct Api {
        create_builder: unsafe extern "C" fn(*mut *mut AAudioStreamBuilder) -> i32,
        delete_builder: unsafe extern "C" fn(*mut AAudioStreamBuilder) -> i32,
        set_direction: BuilderSetter,
        set_sample_rate: BuilderSetter,
        set_channel_count: BuilderSetter,
        set_format: BuilderSetter,
        set_sharing_mode: BuilderSetter,
        set_performance_mode: BuilderSetter,
        set_data_callback:
            unsafe extern "C" fn(*mut AAudioStreamBuilder, DataCallback, *mut c_void),
        set_error_callback:
            unsafe extern "C" fn(*mut AAudioStreamBuilder, ErrorCallback, *mut c_void),
        open_stream: unsafe extern "C" fn(*mut AAudioStreamBuilder, *mut *mut AAudioStream) -> i32,
        request_start: StreamCall,
        request_stop: StreamCall,
        close: StreamCall,
        sample_rate: StreamCall,
        channel_count: StreamCall,
        format: StreamCall,
        frames_per_burst: StreamCall,
        buffer_capacity: StreamCall,
        set_buffer_size: unsafe extern "C" fn(*mut AAudioStream, i32) -> i32,
        result_text: unsafe extern "C" fn(i32) -> *const c_char,
        /// Android 9 (API 28) and later: what turns on the platform's voice processing.
        voice: Option<VoiceApi>,
    }

    struct VoiceApi {
        set_usage: BuilderSetter,
        set_content_type: BuilderSetter,
        set_input_preset: BuilderSetter,
        set_session_id: BuilderSetter,
        session_id: StreamCall,
    }

    /// Looks up `name` in `library`.
    ///
    /// # Safety
    ///
    /// `library` is a live `dlopen` handle and `T` is the function pointer type of the C
    /// declaration of `name`.
    unsafe fn symbol<T: Copy>(library: *mut c_void, name: &CStr) -> Option<T> {
        // SAFETY: `library` is live (caller) and `name` is NUL-terminated.
        let address = unsafe { libc::dlsym(library, name.as_ptr()) };
        if address.is_null() {
            return None;
        }
        // SAFETY: `T` is a function pointer (caller), the same size as a data pointer on
        // Android, and `address` is that function.
        Some(unsafe { std::mem::transmute_copy::<*mut c_void, T>(&address) })
    }

    impl Api {
        /// AAudio, loaded once per process.
        fn get() -> Result<&'static Api, AudioError> {
            static API: OnceLock<Result<Api, AudioError>> = OnceLock::new();
            API.get_or_init(Api::load).as_ref().map_err(Clone::clone)
        }

        fn load() -> Result<Api, AudioError> {
            // SAFETY: a NUL-terminated name; the handle is never closed, so every symbol
            // taken from it stays valid for the life of the process.
            let library = unsafe { libc::dlopen(c"libaaudio.so".as_ptr(), libc::RTLD_NOW) };
            if library.is_null() {
                return Err(AudioError::Backend(
                    "AAudio is not available: it needs Android 8.0 (API 26)".to_owned(),
                ));
            }
            macro_rules! required {
                ($name:literal) => {
                    // SAFETY: the field this lands in has the type of the C declaration.
                    unsafe { symbol(library, $name) }
                        .ok_or_else(|| AudioError::Backend(format!("AAudio has no {:?}", $name)))?
                };
            }
            macro_rules! optional {
                ($name:literal) => {
                    // SAFETY: as above.
                    unsafe { symbol(library, $name) }
                };
            }
            let voice = (|| {
                Some(VoiceApi {
                    set_usage: optional!(c"AAudioStreamBuilder_setUsage")?,
                    set_content_type: optional!(c"AAudioStreamBuilder_setContentType")?,
                    set_input_preset: optional!(c"AAudioStreamBuilder_setInputPreset")?,
                    set_session_id: optional!(c"AAudioStreamBuilder_setSessionId")?,
                    session_id: optional!(c"AAudioStream_getSessionId")?,
                })
            })();
            Ok(Api {
                create_builder: required!(c"AAudio_createStreamBuilder"),
                delete_builder: required!(c"AAudioStreamBuilder_delete"),
                set_direction: required!(c"AAudioStreamBuilder_setDirection"),
                set_sample_rate: required!(c"AAudioStreamBuilder_setSampleRate"),
                set_channel_count: required!(c"AAudioStreamBuilder_setChannelCount"),
                set_format: required!(c"AAudioStreamBuilder_setFormat"),
                set_sharing_mode: required!(c"AAudioStreamBuilder_setSharingMode"),
                set_performance_mode: required!(c"AAudioStreamBuilder_setPerformanceMode"),
                set_data_callback: required!(c"AAudioStreamBuilder_setDataCallback"),
                set_error_callback: required!(c"AAudioStreamBuilder_setErrorCallback"),
                open_stream: required!(c"AAudioStreamBuilder_openStream"),
                request_start: required!(c"AAudioStream_requestStart"),
                request_stop: required!(c"AAudioStream_requestStop"),
                close: required!(c"AAudioStream_close"),
                sample_rate: required!(c"AAudioStream_getSampleRate"),
                channel_count: required!(c"AAudioStream_getChannelCount"),
                format: required!(c"AAudioStream_getFormat"),
                frames_per_burst: required!(c"AAudioStream_getFramesPerBurst"),
                buffer_capacity: required!(c"AAudioStream_getBufferCapacityInFrames"),
                set_buffer_size: required!(c"AAudioStream_setBufferSizeInFrames"),
                result_text: required!(c"AAudio_convertResultToText"),
                voice,
            })
        }

        /// An error for a failed call, with AAudio's name for the result.
        fn error(&self, what: &str, result: i32) -> AudioError {
            // SAFETY: AAudio returns a static, NUL-terminated string for any value.
            let text = unsafe { (self.result_text)(result) };
            let name = if text.is_null() {
                result.to_string()
            } else {
                // SAFETY: not null, and static and NUL-terminated as above.
                unsafe { CStr::from_ptr(text) }
                    .to_string_lossy()
                    .into_owned()
            };
            AudioError::Backend(format!("AAudio {what}: {name}"))
        }
    }

    /// A stream builder, deleted when dropped.
    struct Builder {
        api: &'static Api,
        raw: NonNull<AAudioStreamBuilder>,
    }

    impl Builder {
        fn new(api: &'static Api) -> Result<Self, AudioError> {
            let mut raw = ptr::null_mut();
            // SAFETY: `raw` is a valid place for the new builder.
            let result = unsafe { (api.create_builder)(&mut raw) };
            match NonNull::new(raw) {
                Some(raw) if result == OK => Ok(Self { api, raw }),
                _ => Err(api.error("create builder", result)),
            }
        }

        fn set(&self, setter: BuilderSetter, value: i32) {
            // SAFETY: the builder is live until `drop`.
            unsafe { setter(self.raw.as_ptr(), value) }
        }
    }

    impl Drop for Builder {
        fn drop(&mut self) {
            // SAFETY: live and deleted once; streams opened from it do not need it.
            unsafe { (self.api.delete_builder)(self.raw.as_ptr()) };
        }
    }

    /// One open AAudio stream and the callback state it points to. Dropping it stops and closes
    /// the stream first and frees the callback state after, so AAudio never sees freed memory.
    pub(super) struct Stream {
        api: &'static Api,
        /// `None` once closed.
        raw: Option<NonNull<AAudioStream>>,
        /// From `Box::into_raw`; freed in `drop`.
        slot: NonNull<CallbackSlot>,
    }

    // SAFETY: AAudio lets any thread but the stream's own callbacks control a stream. The slot
    // is used by the data callback only while the stream runs, and by the owner only when it
    // does not (before `requestStart`, after `close`).
    unsafe impl Send for Stream {}

    impl Stream {
        /// Opens a stream, not yet started, asking for the engine's format and a voice call.
        pub(super) fn open(
            direction: Direction,
            session: i32,
            health: &Arc<StreamHealth>,
        ) -> Result<Self, AudioError> {
            let api = Api::get()?;
            let builder = Builder::new(api)?;
            builder.set(api.set_direction, direction as i32);
            builder.set(api.set_sample_rate, SAMPLE_RATE as i32);
            builder.set(api.set_channel_count, 1);
            builder.set(api.set_format, FORMAT_PCM_I16);
            builder.set(api.set_sharing_mode, SHARING_MODE_SHARED);
            builder.set(api.set_performance_mode, PERFORMANCE_MODE_LOW_LATENCY);
            if let Some(voice) = &api.voice {
                match direction {
                    Direction::Input => {
                        builder.set(voice.set_input_preset, INPUT_PRESET_VOICE_COMMUNICATION)
                    }
                    Direction::Output => {
                        builder.set(voice.set_usage, USAGE_VOICE_COMMUNICATION);
                        builder.set(voice.set_content_type, CONTENT_TYPE_SPEECH);
                    }
                }
                builder.set(voice.set_session_id, session);
            }

            let slot = NonNull::from(Box::leak(Box::<CallbackSlot>::default()));
            // SAFETY: the builder is live; the slot lives until this stream is dropped, after
            // the AAudio stream is closed; `health` is kept alive by the backend beyond that.
            unsafe {
                (api.set_data_callback)(builder.raw.as_ptr(), on_data, slot.as_ptr().cast());
                (api.set_error_callback)(
                    builder.raw.as_ptr(),
                    on_error,
                    Arc::as_ptr(health).cast_mut().cast(),
                );
            }
            let mut raw = ptr::null_mut();
            // SAFETY: the builder is live and `raw` is a valid place for the stream.
            let result = unsafe { (api.open_stream)(builder.raw.as_ptr(), &mut raw) };
            let stream = Self {
                api,
                raw: NonNull::new(raw),
                slot,
            };
            if result != OK || stream.raw.is_none() {
                return Err(api.error("open stream", result));
            }
            Ok(stream)
        }

        fn call(&self, function: StreamCall) -> i32 {
            match self.raw {
                // SAFETY: open until `shut`, which clears `raw`.
                Some(raw) => unsafe { function(raw.as_ptr()) },
                None => 0,
            }
        }

        /// The format AAudio granted.
        pub(super) fn format(&self) -> Result<(StreamFormat, SampleKind), AudioError> {
            granted_format(
                self.call(self.api.sample_rate),
                self.call(self.api.channel_count),
                self.call(self.api.format),
            )
        }

        /// The effect session AAudio gave the stream, if it knows about sessions.
        pub(super) fn session_id(&self) -> Option<i32> {
            let voice = self.api.voice.as_ref()?;
            Some(self.call(voice.session_id)).filter(|&id| id > 0)
        }

        /// Keeps the playout buffer short: two bursts.
        pub(super) fn shorten_buffer(&self) {
            let burst = self.call(self.api.frames_per_burst);
            let capacity = self.call(self.api.buffer_capacity);
            if let (Some(frames), Some(raw)) = (playout_buffer_frames(burst, capacity), self.raw) {
                // SAFETY: the stream is open. A refusal leaves AAudio's default, which works.
                unsafe { (self.api.set_buffer_size)(raw.as_ptr(), frames) };
            }
        }

        /// Hands the data callback its processor, before starting.
        pub(super) fn install(&mut self, processor: Processor) {
            // SAFETY: not started yet (callers install first), so no callback touches the slot.
            unsafe { (*self.slot.as_ptr()).0 = Some(processor) };
        }

        /// Starts the callbacks.
        pub(super) fn start(&mut self) -> Result<(), AudioError> {
            match self.call(self.api.request_start) {
                OK => Ok(()),
                result => Err(self.api.error("start", result)),
            }
        }

        /// Stops and closes the AAudio stream. When it returns, no callback runs any more.
        fn shut(&mut self) {
            if let Some(raw) = self.raw.take() {
                // SAFETY: open until here, and never used again. `close` stops the stream too;
                // the explicit stop is for Android 8, where closing a running stream crashed
                // on some devices.
                unsafe {
                    (self.api.request_stop)(raw.as_ptr());
                    (self.api.close)(raw.as_ptr());
                }
            }
        }

        /// Stops and closes the stream, and gives the processor back.
        pub(super) fn close(mut self) -> Option<Processor> {
            self.shut();
            // SAFETY: closed, so no callback uses the slot any more.
            unsafe { (*self.slot.as_ptr()).0.take() }
        }
    }

    impl Drop for Stream {
        fn drop(&mut self) {
            self.shut();
            // SAFETY: from `Box::leak` in `open`; the stream is closed, so AAudio holds no
            // pointer to it any more, and it is freed only here.
            drop(unsafe { Box::from_raw(self.slot.as_ptr()) });
        }
    }

    /// The two streams of a running call.
    struct Streams {
        input: Stream,
        output: Stream,
    }

    impl Streams {
        /// Stops and closes both streams, then gives the rings back.
        fn close(self) -> Option<DeviceIo> {
            let Self { input, output } = self;
            let input = input.close();
            let output = output.close();
            match (input, output) {
                (
                    Some(Processor::Capture {
                        adapter: capture, ..
                    }),
                    Some(Processor::Playout {
                        adapter: playout, ..
                    }),
                ) => Some(DeviceIo {
                    capture: capture.into_ring(),
                    playout: playout.into_ring(),
                }),
                _ => None,
            }
        }
    }

    /// Android's microphone and speaker through AAudio, with the platform's voice processing.
    ///
    /// See the module documentation for what the app must do before [`AudioBackend::start`].
    pub struct AaudioBackend {
        streams: Option<Streams>,
        /// The rings, while a restart failed and waits to be retried.
        parked: Option<DeviceIo>,
        /// Shared with the error callbacks of both streams; outlives them.
        pub(super) health: Arc<StreamHealth>,
        capture_dropped: Counter,
        playout_underruns: Counter,
        session_id: Option<i32>,
        voice_processing: bool,
    }

    impl AaudioBackend {
        /// Loads AAudio. Fails on Android 7 (API 24 and 25), which has none.
        pub fn new() -> Result<Self, AudioError> {
            let api = Api::get()?;
            Ok(Self {
                streams: None,
                parked: None,
                health: Arc::default(),
                capture_dropped: Counter::default(),
                playout_underruns: Counter::default(),
                session_id: None,
                voice_processing: api.voice.is_some(),
            })
        }

        /// Whether the streams ask for the platform's voice processing (echo cancellation,
        /// noise suppression). Only Android 9 (API 28) and later can; on 8.x the call works
        /// without it.
        pub fn voice_processing(&self) -> bool {
            self.voice_processing
        }

        /// The audio session both streams share while running, for Java's `AudioEffect`s.
        pub fn session_id(&self) -> Option<i32> {
            self.session_id
        }

        /// Captured samples lost because the engine did not read in time, since the streams
        /// last opened.
        pub fn capture_dropped(&self) -> Counter {
            self.capture_dropped.clone()
        }

        /// Speaker callbacks that played silence, since the streams last opened.
        pub fn playout_underruns(&self) -> Counter {
            self.playout_underruns.clone()
        }

        /// Errors AAudio reported on either stream, such as a disconnected device.
        pub fn stream_errors(&self) -> Counter {
            self.health.errors.clone()
        }

        /// Reopens both streams if one died (a headset plugged in or out, for example), keeping
        /// the rings. AAudio forbids doing it from its error callback, so the owner polls this,
        /// say every 100 ms. `Ok(true)` if it reopened. If reopening fails, the error says why,
        /// the backend keeps the rings and the next call tries again.
        pub fn restart_if_needed(&mut self) -> Result<bool, AudioError> {
            if !self.health.take_restart() {
                return Ok(false);
            }
            let io = self
                .streams
                .take()
                .and_then(Streams::close)
                .or_else(|| self.parked.take());
            let Some(io) = io else {
                return Ok(false);
            };
            match self.open(io) {
                Ok(()) => Ok(true),
                Err((error, io)) => {
                    if io.is_some() {
                        self.parked = io;
                        self.health.request_restart();
                    }
                    Err(error)
                }
            }
        }

        /// Opens both streams on `io`: the input first, with a new effect session, and the
        /// output in the same session so the echo canceller pairs them. On failure, gives the
        /// rings back when it still has them.
        fn open(&mut self, io: DeviceIo) -> Result<(), (AudioError, Option<DeviceIo>)> {
            let opened = Stream::open(Direction::Input, SESSION_ID_ALLOCATE, &self.health)
                .and_then(|input| {
                    let session = input.session_id();
                    let output = Stream::open(
                        Direction::Output,
                        session.unwrap_or(SESSION_ID_ALLOCATE),
                        &self.health,
                    )?;
                    output.shorten_buffer();
                    let input_format = input.format()?;
                    let output_format = output.format()?;
                    Ok((input, output, input_format, output_format, session))
                });
            let (input, output, (input_format, input_kind), (output_format, output_kind), session) =
                match opened {
                    Ok(opened) => opened,
                    Err(error) => return Err((error, Some(io))),
                };

            // Cannot fail: `granted_format` checked the rate and the channels.
            let adapters = CaptureAdapter::new(input_format, io.capture)
                .and_then(|capture| Ok((capture, PlayoutAdapter::new(output_format, io.playout)?)));
            let (capture, playout) = adapters.map_err(|error| (error, None))?;
            self.capture_dropped = capture.dropped();
            self.playout_underruns = playout.underruns();

            let mut streams = Streams { input, output };
            streams.input.install(Processor::Capture {
                adapter: capture,
                kind: input_kind,
                channels: usize::from(input_format.channels),
            });
            streams.output.install(Processor::Playout {
                adapter: playout,
                kind: output_kind,
                channels: usize::from(output_format.channels),
            });
            if let Err(error) = streams.input.start().and_then(|()| streams.output.start()) {
                return Err((error, streams.close()));
            }
            self.session_id = session;
            self.streams = Some(streams);
            Ok(())
        }
    }

    impl AudioBackend for AaudioBackend {
        fn start(&mut self, io: DeviceIo) -> Result<(), AudioError> {
            self.stop()?;
            self.health.take_restart();
            self.open(io).map_err(|(error, _)| error)
        }

        /// Stops and closes both streams, then drops the adapters and the rings.
        fn stop(&mut self) -> Result<(), AudioError> {
            if let Some(streams) = self.streams.take() {
                drop(streams.close());
            }
            self.parked = None;
            self.session_id = None;
            Ok(())
        }

        /// Reopens both streams if a device change killed them: see
        /// [`AaudioBackend::restart_if_needed`].
        fn maintain(&mut self) -> Result<(), AudioError> {
            self.restart_if_needed().map(|_| ())
        }
    }

    impl Drop for AaudioBackend {
        fn drop(&mut self) {
            // Closes the streams before `health`, which their error callbacks point to, goes.
            let _ = self.stop();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SAMPLE_RATE;
    use crate::audio::ring::{RingConsumer, ring};
    use std::ptr;

    const ENGINE_MONO: StreamFormat = StreamFormat {
        sample_rate: SAMPLE_RATE,
        channels: 1,
    };

    fn drain(consumer: &mut RingConsumer) -> Vec<i16> {
        let mut all = vec![0; consumer.len()];
        consumer.pop(&mut all);
        all
    }

    fn capture_slot(format: StreamFormat, kind: SampleKind) -> (CallbackSlot, RingConsumer) {
        let (producer, consumer) = ring(64);
        let processor = Processor::Capture {
            adapter: CaptureAdapter::new(format, producer).unwrap(),
            kind,
            channels: usize::from(format.channels),
        };
        (CallbackSlot(Some(processor)), consumer)
    }

    fn playout_slot(format: StreamFormat, kind: SampleKind, queued: &[i16]) -> CallbackSlot {
        let (mut producer, consumer) = ring(64);
        producer.push(queued);
        let processor = Processor::Playout {
            adapter: PlayoutAdapter::new(format, consumer).unwrap(),
            kind,
            channels: usize::from(format.channels),
        };
        CallbackSlot(Some(processor))
    }

    /// Calls the data callback as AAudio would, with `slot` as user data.
    fn run_callback<S>(slot: &mut CallbackSlot, buffer: &mut [S], frames: i32) -> i32 {
        // SAFETY: the slot and the buffer outlive the call, and `frames` fits the buffer.
        unsafe {
            on_data(
                ptr::null_mut(),
                ptr::from_mut(slot).cast(),
                buffer.as_mut_ptr().cast(),
                frames,
            )
        }
    }

    #[test]
    fn the_capture_callback_copies_i16_mono_into_the_ring() {
        let (mut slot, mut consumer) = capture_slot(ENGINE_MONO, SampleKind::I16);
        let mut buffer = [10i16, -20, 30];
        assert_eq!(
            run_callback(&mut slot, &mut buffer, 3),
            CALLBACK_RESULT_CONTINUE
        );
        assert_eq!(drain(&mut consumer), [10, -20, 30]);
    }

    #[test]
    fn the_capture_callback_reads_float_stereo_frames() {
        let stereo = StreamFormat {
            sample_rate: SAMPLE_RATE,
            channels: 2,
        };
        let (mut slot, mut consumer) = capture_slot(stereo, SampleKind::F32);
        let mut buffer = [0.5f32, 0.0, -0.25, -0.25];
        run_callback(&mut slot, &mut buffer, 2);
        assert_eq!(drain(&mut consumer), [8192, -8192]);
    }

    #[test]
    fn the_playout_callback_fills_i16_with_silence_after_the_ring_runs_dry() {
        let mut slot = playout_slot(ENGINE_MONO, SampleKind::I16, &[1, 2]);
        let mut buffer = [7i16; 4];
        assert_eq!(
            run_callback(&mut slot, &mut buffer, 4),
            CALLBACK_RESULT_CONTINUE
        );
        assert_eq!(buffer, [1, 2, 0, 0]);
    }

    #[test]
    fn the_playout_callback_fills_float_stereo_frames() {
        let stereo = StreamFormat {
            sample_rate: SAMPLE_RATE,
            channels: 2,
        };
        let mut slot = playout_slot(stereo, SampleKind::F32, &[16384, -8192]);
        let mut buffer = [9.0f32; 4];
        run_callback(&mut slot, &mut buffer, 2);
        assert_eq!(buffer, [0.5, 0.5, -0.25, -0.25]);
    }

    #[test]
    fn the_callback_only_touches_the_frames_it_is_given() {
        let mut slot = playout_slot(ENGINE_MONO, SampleKind::I16, &[1, 2, 3]);
        let mut buffer = [7i16; 3];
        run_callback(&mut slot, &mut buffer, 2);
        assert_eq!(buffer, [1, 2, 7]);
    }

    #[test]
    fn the_callback_ignores_missing_or_empty_buffers() {
        let mut slot = playout_slot(ENGINE_MONO, SampleKind::I16, &[1, 2]);
        let mut buffer = [7i16; 2];
        for frames in [0, -5] {
            assert_eq!(
                run_callback(&mut slot, &mut buffer, frames),
                CALLBACK_RESULT_CONTINUE
            );
        }
        // SAFETY: a null buffer and a null slot are what the callback must survive.
        let results = unsafe {
            [
                on_data(
                    ptr::null_mut(),
                    ptr::from_mut(&mut slot).cast(),
                    ptr::null_mut(),
                    2,
                ),
                on_data(
                    ptr::null_mut(),
                    ptr::null_mut(),
                    buffer.as_mut_ptr().cast(),
                    2,
                ),
            ]
        };
        assert_eq!(results, [CALLBACK_RESULT_CONTINUE; 2]);
        assert_eq!(buffer, [7, 7]);

        // Nothing was consumed: the queued samples are still there.
        run_callback(&mut slot, &mut buffer, 2);
        assert_eq!(buffer, [1, 2]);
    }

    #[test]
    fn an_empty_slot_leaves_the_buffer_alone() {
        let mut slot = CallbackSlot::default();
        let mut buffer = [7i16; 2];
        assert_eq!(
            run_callback(&mut slot, &mut buffer, 2),
            CALLBACK_RESULT_CONTINUE
        );
        assert_eq!(buffer, [7, 7]);
    }

    #[test]
    fn the_playout_buffer_holds_two_bursts() {
        assert_eq!(playout_buffer_frames(192, 4096), Some(384));
    }

    #[test]
    fn the_playout_buffer_never_asks_for_more_than_the_capacity() {
        assert_eq!(playout_buffer_frames(192, 300), Some(300));
    }

    #[test]
    fn without_a_burst_size_the_default_buffer_stays() {
        assert_eq!(playout_buffer_frames(0, 4096), None);
        assert_eq!(playout_buffer_frames(-1, 4096), None);
        assert_eq!(playout_buffer_frames(i32::MAX, i32::MAX), Some(i32::MAX));
    }

    // On the device: `cargo test --target aarch64-linux-android --no-run`, push the test binary
    // to /data/local/tmp and run it with `--ignored audio::android` from `adb shell`.
    #[cfg(target_os = "android")]
    mod device {
        use super::super::device::{Direction, SESSION_ID_ALLOCATE, Stream};
        use super::*;
        use crate::audio::{AudioBackend, audio_io};
        use crate::{FRAME_SAMPLES, Frame};
        use std::sync::Arc;
        use std::time::Duration;

        #[test]
        fn the_backend_can_move_to_another_thread() {
            fn assert_send<T: Send>() {}
            assert_send::<AaudioBackend>();
        }

        #[test]
        fn aaudio_loads() {
            AaudioBackend::new().unwrap();
        }

        // Nothing is open, so the restart has no streams to reopen; what counts is that
        // `maintain` took the request, as `restart_if_needed` does.
        #[test]
        fn maintain_takes_up_a_pending_restart() {
            let mut backend = AaudioBackend::new().unwrap();
            backend.health.report();
            backend.maintain().unwrap();
            assert!(!backend.health.take_restart());
        }

        #[test]
        fn the_platform_backend_loads_on_android() {
            let mut backend = crate::audio::platform_backend().unwrap();
            assert_eq!(backend.maintain(), Ok(()));
        }

        // Plays a quiet 440 Hz tone for one second through the voice-call output.
        #[test]
        #[ignore]
        fn plays_a_quiet_tone_for_one_second() {
            let health = Arc::new(StreamHealth::default());
            let mut stream = Stream::open(Direction::Output, SESSION_ID_ALLOCATE, &health).unwrap();
            let (format, kind) = stream.format().unwrap();
            eprintln!("output granted: {format:?} {kind:?}");

            let (mut producer, consumer) = ring(60 * FRAME_SAMPLES);
            let tone: Vec<i16> = (0..SAMPLE_RATE as usize)
                .map(|n| {
                    let phase = std::f32::consts::TAU * 440.0 * n as f32 / SAMPLE_RATE as f32;
                    (phase.sin() * 3000.0) as i16
                })
                .collect();
            assert_eq!(producer.push(&tone), tone.len());
            let adapter = PlayoutAdapter::new(format, consumer).unwrap();
            let underruns = adapter.underruns();
            stream.install(Processor::Playout {
                adapter,
                kind,
                channels: usize::from(format.channels),
            });
            stream.start().unwrap();
            std::thread::sleep(Duration::from_millis(1100));
            assert!(stream.close().is_some());

            assert!(producer.is_empty(), "{} samples left", producer.len());
            eprintln!("underruns: {}", underruns.get());
            assert_eq!(health.errors.get(), 0);
        }

        // Both streams through the backend. From `adb shell` the microphone may be refused.
        #[test]
        #[ignore]
        fn captures_and_plays_through_the_backend() {
            let (device, mut engine) = audio_io(16);
            let mut backend = AaudioBackend::new().unwrap();
            backend.start(device).unwrap();
            assert!(backend.voice_processing());
            assert!(backend.session_id().is_some());

            let silence: Frame = [0; FRAME_SAMPLES];
            let mut frame = [0; FRAME_SAMPLES];
            let mut captured = 0;
            for _ in 0..50 {
                engine.playout.write_frame(&silence);
                while engine.capture.read_frame(&mut frame) {
                    captured += 1;
                }
                assert!(!backend.restart_if_needed().unwrap());
                std::thread::sleep(Duration::from_millis(20));
            }
            eprintln!(
                "session {:?}, {captured} frames captured, {} dropped, {} underruns, {} errors",
                backend.session_id(),
                backend.capture_dropped().get(),
                backend.playout_underruns().get(),
                backend.stream_errors().get(),
            );
            backend.stop().unwrap();
            backend.stop().unwrap();
            assert_eq!(backend.session_id(), None);
            assert!(captured >= 30, "{captured} frames in 1 s");
        }

        // What a disconnect does, without unplugging anything: the error callback's report.
        #[test]
        #[ignore]
        fn a_reported_error_reopens_both_streams_on_the_same_rings() {
            let (device, mut engine) = audio_io(16);
            let mut backend = AaudioBackend::new().unwrap();
            backend.start(device).unwrap();
            let first_session = backend.session_id();
            assert!(!backend.restart_if_needed().unwrap());

            backend.health.report();
            assert!(backend.restart_if_needed().unwrap());
            assert!(!backend.restart_if_needed().unwrap());
            eprintln!("session {first_session:?} -> {:?}", backend.session_id());
            assert!(backend.session_id().is_some());

            std::thread::sleep(Duration::from_millis(500));
            let mut frame = [0; FRAME_SAMPLES];
            let mut captured = 0;
            while engine.capture.read_frame(&mut frame) {
                captured += 1;
            }
            assert!(captured > 0);
            backend.stop().unwrap();
        }
    }

    #[test]
    fn the_error_callback_counts_the_error_and_asks_once_for_a_restart() {
        let health = StreamHealth::default();
        let errors = health.errors.clone();
        assert!(!health.take_restart());

        // SAFETY: `health` outlives both calls; a null one must be survived.
        unsafe {
            on_error(
                ptr::null_mut(),
                ptr::from_ref(&health).cast_mut().cast(),
                -899,
            );
            on_error(ptr::null_mut(), ptr::null_mut(), -899);
        }
        assert_eq!(errors.get(), 1);
        assert!(health.take_restart());
        assert!(!health.take_restart());
    }

    #[test]
    fn the_engine_format_is_taken_as_it_is() {
        let granted = granted_format(48_000, 1, FORMAT_PCM_I16).unwrap();
        assert_eq!(
            granted,
            (
                StreamFormat {
                    sample_rate: SAMPLE_RATE,
                    channels: 1
                },
                SampleKind::I16
            )
        );
    }

    #[test]
    fn float_and_other_rates_and_channels_are_taken_for_the_adapters_to_convert() {
        let granted = granted_format(44_100, 2, FORMAT_PCM_FLOAT).unwrap();
        assert_eq!(
            granted,
            (
                StreamFormat {
                    sample_rate: 44_100,
                    channels: 2
                },
                SampleKind::F32
            )
        );
    }

    #[test]
    fn sample_types_the_adapters_cannot_read_are_rejected() {
        // Unspecified, 24-bit packed and 32-bit integer.
        for format in [0, 3, 4] {
            assert!(matches!(
                granted_format(48_000, 1, format),
                Err(AudioError::Backend(_))
            ));
        }
    }

    #[test]
    fn a_rate_or_channel_count_that_makes_no_sense_is_rejected() {
        for (rate, channels) in [
            (0, 1),
            (-48_000, 1),
            (48_000, 0),
            (48_000, -1),
            (48_000, 70_000),
        ] {
            assert!(matches!(
                granted_format(rate, channels, FORMAT_PCM_I16),
                Err(AudioError::InvalidFormat { .. })
            ));
        }
    }
}
