//! iOS backend on the `VoiceProcessingIO` audio unit.
//!
//! [`VoiceProcessingBackend`] (iOS only) runs Apple's voice-processing I/O unit: echo
//! cancellation, noise suppression and automatic gain control done by the system, the
//! microphone on bus 1 and the speaker, receiver or headset on bus 0.
//!
//! # Contract with the app (Swift and CallKit)
//!
//! **This backend never configures or activates `AVAudioSession`.** The session belongs to the
//! app, and with CallKit the system activates it:
//!
//! 1. The app's Swift code sets the category and mode before the call is reported or answered:
//!    `.playAndRecord` with mode `.voiceChat` (options such as `.allowBluetooth` are the app's
//!    choice). It does not call `setActive(true)` itself: CallKit does.
//! 2. In `CXProviderDelegate.provider(_:didActivate:)` the app calls
//!    [`AudioBackend::start`](super::AudioBackend::start). Not before: without an active
//!    session that can record, the unit initialises but refuses to start (OSStatus -66637).
//! 3. In `provider(_:didDeactivate:)`, and when the call ends, the app calls
//!    [`AudioBackend::stop`](super::AudioBackend::stop). If an interruption ends and CallKit
//!    activates the session again, the app stops and starts the backend again.
//! 4. The app declares `NSMicrophoneUsageDescription` and the `audio` and `voip` background
//!    modes. Only one voice-processing unit may run per process, so the WebView must not hold
//!    the microphone during a native call.
//!
//! Routes (speaker, receiver, Bluetooth) are the session's business too: the unit follows them.
//!
//! # Formats
//!
//! The backend asks for 48 kHz mono i16 on both sides (the engine's own format, copied bit for
//! bit), or f32 if the unit refuses i16. After `AudioUnitInitialize` it reads back what the unit
//! actually gives and hands that to the [`CaptureAdapter`] and [`PlayoutAdapter`], which convert
//! any rate, interleaved channel count and i16 or f32.
//!
//! # Real time
//!
//! The callbacks take no lock, allocate nothing and make no system call besides
//! `AudioUnitRender`. The microphone buffer is allocated at start for the unit's maximum slice,
//! which the backend raises to 4096 frames, what iOS uses with the screen locked. A callback
//! that fails is counted ([`VoiceProcessingBackend::capture_errors`]), never raised. `stop`
//! stops and uninitialises the unit before the callback state is freed, so nothing is freed on
//! the audio thread; `Drop` does the same and then disposes of the unit.
//!
//! # FFI
//!
//! The dozen AudioToolbox declarations are written by hand, from the SDK headers, with tests
//! pinning the struct layouts: `coreaudio-sys` would need bindgen and libclang at build time,
//! and `objc2-audio-toolbox` brings a family of generated crates, both for ten functions.

use std::ffi::c_void;
use std::mem::size_of;

use super::format::Sample;
use super::{
    AudioError, CaptureAdapter, Counter, PlayoutAdapter, RingConsumer, RingProducer, SAMPLE_RATE,
    StreamFormat,
};

// The few AudioToolbox / CoreAudioTypes declarations this backend needs, copied from the iOS SDK
// headers (`AUComponent.h`, `AudioUnitProperties.h`, `CoreAudioBaseTypes.h`). The layout tests
// below pin their sizes.

/// A Core Audio result code: zero is success.
pub(crate) type OSStatus = i32;

const K_AUDIO_FORMAT_LINEAR_PCM: u32 = u32::from_be_bytes(*b"lpcm");
const K_AUDIO_FORMAT_FLAG_IS_FLOAT: u32 = 1 << 0;
const K_AUDIO_FORMAT_FLAG_IS_BIG_ENDIAN: u32 = 1 << 1;
const K_AUDIO_FORMAT_FLAG_IS_SIGNED_INTEGER: u32 = 1 << 2;
const K_AUDIO_FORMAT_FLAG_IS_PACKED: u32 = 1 << 3;
const K_AUDIO_FORMAT_FLAG_IS_NON_INTERLEAVED: u32 = 1 << 5;

/// `AudioStreamBasicDescription`: the layout of a PCM stream.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct AudioStreamBasicDescription {
    pub sample_rate: f64,
    pub format_id: u32,
    pub format_flags: u32,
    pub bytes_per_packet: u32,
    pub frames_per_packet: u32,
    pub bytes_per_frame: u32,
    pub channels_per_frame: u32,
    pub bits_per_channel: u32,
    pub reserved: u32,
}

/// `AudioBuffer`: one buffer of samples, owned by whoever filled in `data`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct AudioBuffer {
    pub number_channels: u32,
    pub data_byte_size: u32,
    pub data: *mut c_void,
}

/// `AudioBufferList`: in C a variable-length array; the callbacks here see one buffer, since
/// the formats they accept are interleaved.
#[repr(C)]
#[derive(Debug)]
pub(crate) struct AudioBufferList {
    pub number_buffers: u32,
    pub buffers: [AudioBuffer; 1],
}

/// `AudioComponentDescription`: which audio unit to look for.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct AudioComponentDescription {
    pub component_type: u32,
    pub component_sub_type: u32,
    pub component_manufacturer: u32,
    pub component_flags: u32,
    pub component_flags_mask: u32,
}

/// `AURenderCallback`: the input and render callbacks.
pub(crate) type AURenderCallback = unsafe extern "C" fn(
    ref_con: *mut c_void,
    action_flags: *mut u32,
    time_stamp: *const AudioTimeStamp,
    bus: u32,
    frames: u32,
    data: *mut AudioBufferList,
) -> OSStatus;

/// `AURenderCallbackStruct`: a callback and the pointer it gets back.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct AURenderCallbackStruct {
    pub input_proc: Option<AURenderCallback>,
    pub input_proc_ref_con: *mut c_void,
}

/// The sample type of a stream the adapters can take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SampleKind {
    I16,
    F32,
}

impl SampleKind {
    /// Bytes in one sample.
    pub(crate) fn bytes(self) -> u32 {
        match self {
            Self::I16 => 2,
            Self::F32 => 4,
        }
    }
}

/// The stream this backend asks the unit for, on both sides: 48 kHz mono, packed and
/// native-endian, in `kind` samples.
pub(crate) fn requested_format(kind: SampleKind) -> AudioStreamBasicDescription {
    let flags = match kind {
        SampleKind::I16 => K_AUDIO_FORMAT_FLAG_IS_SIGNED_INTEGER,
        SampleKind::F32 => K_AUDIO_FORMAT_FLAG_IS_FLOAT,
    };
    let bytes = kind.bytes();
    AudioStreamBasicDescription {
        sample_rate: f64::from(SAMPLE_RATE),
        format_id: K_AUDIO_FORMAT_LINEAR_PCM,
        format_flags: flags | K_AUDIO_FORMAT_FLAG_IS_PACKED,
        bytes_per_packet: bytes,
        frames_per_packet: 1,
        bytes_per_frame: bytes,
        channels_per_frame: 1,
        bits_per_channel: 8 * bytes,
        reserved: 0,
    }
}

/// A stream the adapters can take, as the unit reports it after initialising.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DeviceFormat {
    pub stream: StreamFormat,
    pub kind: SampleKind,
}

/// Reads a stream description back into something the adapters take: linear PCM in
/// native-endian i16 or f32, interleaved (or mono), at any whole sample rate.
pub(crate) fn device_format(
    description: &AudioStreamBasicDescription,
) -> Result<DeviceFormat, AudioError> {
    let invalid = || AudioError::InvalidFormat {
        // `as` saturates, and NaN becomes 0: only for the message.
        sample_rate: description.sample_rate as u32,
        channels: u16::try_from(description.channels_per_frame).unwrap_or(u16::MAX),
    };
    let flags = description.format_flags;
    let kind = match (
        flags & (K_AUDIO_FORMAT_FLAG_IS_FLOAT | K_AUDIO_FORMAT_FLAG_IS_SIGNED_INTEGER),
        description.bits_per_channel,
    ) {
        (K_AUDIO_FORMAT_FLAG_IS_SIGNED_INTEGER, 16) => SampleKind::I16,
        (K_AUDIO_FORMAT_FLAG_IS_FLOAT, 32) => SampleKind::F32,
        _ => return Err(invalid()),
    };
    let channels = u16::try_from(description.channels_per_frame).map_err(|_| invalid())?;
    let rate = description.sample_rate;
    let whole_rate = rate.is_finite() && rate >= 1.0 && rate <= f64::from(u32::MAX);
    let interleaved = flags & K_AUDIO_FORMAT_FLAG_IS_NON_INTERLEAVED == 0 || channels == 1;
    let packed = description.bytes_per_frame == u32::from(channels) * kind.bytes();
    if description.format_id != K_AUDIO_FORMAT_LINEAR_PCM
        || flags & K_AUDIO_FORMAT_FLAG_IS_BIG_ENDIAN != 0
        || channels == 0
        || !whole_rate
        || rate.fract() != 0.0
        || !interleaved
        || !packed
    {
        return Err(invalid());
    }
    Ok(DeviceFormat {
        stream: StreamFormat {
            sample_rate: rate as u32,
            channels,
        },
        kind,
    })
}

/// Microphone samples in the unit's sample type, allocated once when the unit starts.
enum SampleBuffer {
    I16(Box<[i16]>),
    F32(Box<[f32]>),
}

impl SampleBuffer {
    fn new(kind: SampleKind, samples: usize) -> Self {
        match kind {
            SampleKind::I16 => Self::I16(vec![0; samples].into_boxed_slice()),
            SampleKind::F32 => Self::F32(vec![0.0; samples].into_boxed_slice()),
        }
    }

    fn len(&self) -> usize {
        match self {
            Self::I16(samples) => samples.len(),
            Self::F32(samples) => samples.len(),
        }
    }

    fn sample_bytes(&self) -> usize {
        match self {
            Self::I16(_) => size_of::<i16>(),
            Self::F32(_) => size_of::<f32>(),
        }
    }

    fn as_mut_ptr(&mut self) -> *mut c_void {
        match self {
            Self::I16(samples) => samples.as_mut_ptr().cast(),
            Self::F32(samples) => samples.as_mut_ptr().cast(),
        }
    }

    /// Hands the first `count` samples to `adapter`.
    fn push_to(&self, adapter: &mut CaptureAdapter, count: usize) {
        match self {
            Self::I16(samples) => adapter.push(&samples[..count.min(samples.len())]),
            Self::F32(samples) => adapter.push(&samples[..count.min(samples.len())]),
        }
    }
}

/// The capture half of the unit's input callback: renders the microphone into a pre-allocated
/// buffer and hands it to a [`CaptureAdapter`].
pub(crate) struct Capture {
    adapter: CaptureAdapter,
    buffer: SampleBuffer,
    channels: u32,
    errors: Counter,
}

impl Capture {
    /// A capture for `format` that can take callbacks of up to `max_frames` frames.
    pub(crate) fn new(
        format: DeviceFormat,
        max_frames: u32,
        producer: RingProducer,
    ) -> Result<Self, AudioError> {
        let channels = u32::from(format.stream.channels);
        // The list tells the unit the buffer's size in bytes as a u32.
        let samples = max_frames
            .checked_mul(channels)
            .filter(|samples| samples.checked_mul(format.kind.bytes()).is_some())
            .ok_or_else(|| {
                AudioError::Backend(format!("{max_frames} frames per callback is too many"))
            })?;
        Ok(Self {
            adapter: CaptureAdapter::new(format.stream, producer)?,
            buffer: SampleBuffer::new(format.kind, samples as usize),
            channels,
            errors: Counter::default(),
        })
    }

    /// How many callbacks failed to render or were larger than the buffer.
    pub(crate) fn errors(&self) -> Counter {
        self.errors.clone()
    }

    /// How many engine samples were dropped because the ring was full.
    pub(crate) fn dropped(&self) -> Counter {
        self.adapter.dropped()
    }

    /// Runs on the audio thread for every input callback: `render` (`AudioUnitRender` on the
    /// unit) fills `frames` frames into the buffer the list points at, and they go to the ring.
    /// No allocation, no lock, no system call besides `render` itself.
    pub(crate) fn process(
        &mut self,
        frames: u32,
        render: impl FnOnce(&mut AudioBufferList) -> OSStatus,
    ) {
        let samples = (frames as usize).saturating_mul(self.channels as usize);
        if samples > self.buffer.len() {
            self.errors.add(1);
            return;
        }
        let sample_bytes = self.buffer.sample_bytes();
        let data = self.buffer.as_mut_ptr();
        let mut list = AudioBufferList {
            number_buffers: 1,
            buffers: [AudioBuffer {
                number_channels: self.channels,
                // At most the buffer's size, which `new` checked fits in a u32.
                data_byte_size: (samples * sample_bytes) as u32,
                data,
            }],
        };
        if render(&mut list) != 0 || list.buffers[0].data != data {
            self.errors.add(1);
            return;
        }
        let rendered = (list.buffers[0].data_byte_size as usize / sample_bytes).min(samples);
        self.buffer.push_to(&mut self.adapter, rendered);
    }
}

/// `kAudio_ParamError`, for a callback handed no buffer list.
const K_AUDIO_PARAM_ERROR: OSStatus = -50;

/// `AudioTimeStamp`, only ever passed through by pointer.
#[repr(C)]
pub(crate) struct AudioTimeStamp {
    _opaque: [u8; 0],
}

/// The speaker half of the unit: fills the unit's buffers from a [`PlayoutAdapter`].
pub(crate) struct Playout {
    adapter: PlayoutAdapter,
    kind: SampleKind,
}

impl Playout {
    /// A playout for `format`, reading from `consumer`.
    pub(crate) fn new(format: DeviceFormat, consumer: RingConsumer) -> Result<Self, AudioError> {
        Ok(Self {
            adapter: PlayoutAdapter::new(format.stream, consumer)?,
            kind: format.kind,
        })
    }

    /// How many callbacks ran out of samples and played silence.
    pub(crate) fn underruns(&self) -> Counter {
        self.adapter.underruns()
    }

    /// Fills `list` for one render callback.
    ///
    /// # Safety
    ///
    /// `list` is null or points at a valid `AudioBufferList` with `number_buffers` buffers,
    /// each null or holding `data_byte_size` writable bytes, aligned for the sample type.
    unsafe fn render(&mut self, list: *mut AudioBufferList) -> OSStatus {
        if list.is_null() {
            return K_AUDIO_PARAM_ERROR;
        }
        // SAFETY: `list` is not null, so the caller guarantees it is a valid list.
        let (count, first) = unsafe {
            (
                (*list).number_buffers as usize,
                (&raw mut (*list).buffers).cast::<AudioBuffer>(),
            )
        };
        for index in 0..count {
            // SAFETY: the list holds `count` buffers, one after another from `buffers`.
            let buffer = unsafe { *first.add(index) };
            if buffer.data.is_null() {
                continue;
            }
            // The formats this backend accepts come in one buffer; anything more is silenced.
            let from_ring = index == 0;
            // SAFETY: the caller guarantees `data` holds `data_byte_size` bytes of samples of
            // the unit's format, which is `self.kind`.
            unsafe {
                match self.kind {
                    SampleKind::I16 => fill::<i16>(&mut self.adapter, buffer, from_ring),
                    SampleKind::F32 => fill::<f32>(&mut self.adapter, buffer, from_ring),
                }
            }
        }
        0
    }
}

/// Fills `buffer` from `adapter`, or with silence.
///
/// # Safety
///
/// `buffer.data` is not null and holds `data_byte_size` writable bytes, aligned for `S`.
unsafe fn fill<S: Sample>(adapter: &mut PlayoutAdapter, buffer: AudioBuffer, from_ring: bool) {
    let len = buffer.data_byte_size as usize / size_of::<S>();
    // SAFETY: guaranteed by the caller.
    let samples = unsafe { std::slice::from_raw_parts_mut(buffer.data.cast::<S>(), len) };
    if from_ring {
        adapter.fill(samples);
    } else {
        samples.fill(S::from_f32(0.0));
    }
}

/// The unit's render callback (`AURenderCallback`) on the speaker bus: `ref_con` is the
/// [`Playout`].
///
/// # Safety
///
/// `ref_con` points at a `Playout` that nothing else touches while the callback runs, and
/// `data` is what [`Playout::render`] expects. The unit guarantees both while it is started.
pub(crate) unsafe extern "C" fn render_callback(
    ref_con: *mut c_void,
    _action_flags: *mut u32,
    _time_stamp: *const AudioTimeStamp,
    _bus: u32,
    _frames: u32,
    data: *mut AudioBufferList,
) -> OSStatus {
    // SAFETY: the caller hands the `Playout` registered with the callback, used by this
    // callback alone, and a buffer list as `render` requires.
    unsafe {
        let playout = &mut *ref_con.cast::<Playout>();
        playout.render(data)
    }
}

#[cfg(target_os = "ios")]
pub use unit::VoiceProcessingBackend;

#[cfg(target_os = "ios")]
mod unit {
    use std::ptr;

    use super::super::{AudioBackend, DeviceIo};
    use super::*;

    type AudioComponent = *mut c_void;
    type AudioUnit = *mut c_void;

    const K_AUDIO_UNIT_TYPE_OUTPUT: u32 = u32::from_be_bytes(*b"auou");
    const K_AUDIO_UNIT_SUB_TYPE_VOICE_PROCESSING_IO: u32 = u32::from_be_bytes(*b"vpio");
    const K_AUDIO_UNIT_MANUFACTURER_APPLE: u32 = u32::from_be_bytes(*b"appl");

    const K_AUDIO_UNIT_SCOPE_GLOBAL: u32 = 0;
    const K_AUDIO_UNIT_SCOPE_INPUT: u32 = 1;
    const K_AUDIO_UNIT_SCOPE_OUTPUT: u32 = 2;

    const K_AUDIO_UNIT_PROPERTY_STREAM_FORMAT: u32 = 8;
    const K_AUDIO_UNIT_PROPERTY_MAXIMUM_FRAMES_PER_SLICE: u32 = 14;
    const K_AUDIO_UNIT_PROPERTY_SET_RENDER_CALLBACK: u32 = 23;
    const K_AUDIO_UNIT_PROPERTY_SHOULD_ALLOCATE_BUFFER: u32 = 51;
    const K_AUDIO_OUTPUT_UNIT_PROPERTY_ENABLE_IO: u32 = 2003;
    const K_AUDIO_OUTPUT_UNIT_PROPERTY_SET_INPUT_CALLBACK: u32 = 2005;

    /// Element 1 of an I/O unit is the microphone; its output scope is what the app reads.
    const INPUT_BUS: u32 = 1;
    /// Element 0 is the speaker; its input scope is what the app plays.
    const OUTPUT_BUS: u32 = 0;
    /// Apple's advice for a unit that must keep running with the screen locked, where iOS
    /// asks for larger slices (4096 frames) than the default 1156.
    const MAX_FRAMES_PER_SLICE: u32 = 4096;

    #[link(name = "AudioToolbox", kind = "framework")]
    unsafe extern "C" {
        fn AudioComponentFindNext(
            component: AudioComponent,
            description: *const AudioComponentDescription,
        ) -> AudioComponent;
        fn AudioComponentInstanceNew(component: AudioComponent, unit: *mut AudioUnit) -> OSStatus;
        fn AudioComponentInstanceDispose(unit: AudioUnit) -> OSStatus;
        fn AudioUnitSetProperty(
            unit: AudioUnit,
            id: u32,
            scope: u32,
            element: u32,
            data: *const c_void,
            size: u32,
        ) -> OSStatus;
        fn AudioUnitGetProperty(
            unit: AudioUnit,
            id: u32,
            scope: u32,
            element: u32,
            data: *mut c_void,
            size: *mut u32,
        ) -> OSStatus;
        fn AudioUnitInitialize(unit: AudioUnit) -> OSStatus;
        fn AudioUnitUninitialize(unit: AudioUnit) -> OSStatus;
        fn AudioOutputUnitStart(unit: AudioUnit) -> OSStatus;
        fn AudioOutputUnitStop(unit: AudioUnit) -> OSStatus;
        fn AudioUnitRender(
            unit: AudioUnit,
            action_flags: *mut u32,
            time_stamp: *const AudioTimeStamp,
            bus: u32,
            frames: u32,
            data: *mut AudioBufferList,
        ) -> OSStatus;
    }

    /// What the input callback needs: the unit, to render the microphone from, and the capture.
    struct Input {
        unit: AudioUnit,
        capture: Capture,
    }

    /// The unit's input callback: the microphone has `frames` frames ready. `ref_con` is the
    /// [`Input`]; `data` is null, the samples are fetched with `AudioUnitRender`.
    unsafe extern "C" fn input_callback(
        ref_con: *mut c_void,
        action_flags: *mut u32,
        time_stamp: *const AudioTimeStamp,
        bus: u32,
        frames: u32,
        _data: *mut AudioBufferList,
    ) -> OSStatus {
        // SAFETY: `ref_con` is the boxed `Input` registered in `start`, which outlives the
        // running unit and is only touched by this callback while it runs.
        let input = unsafe { &mut *ref_con.cast::<Input>() };
        let unit = input.unit;
        input.capture.process(frames, |list| {
            // SAFETY: the flags and time stamp are the unit's own for this callback, and
            // `list` points at the capture's buffer, sized for `frames` frames.
            unsafe { AudioUnitRender(unit, action_flags, time_stamp, bus, frames, list) }
        });
        0
    }

    /// The callback state of a started unit. The unit holds raw pointers into both boxes, so
    /// they are freed only once the unit can no longer call back.
    struct Running {
        _input: Box<Input>,
        _playout: Box<Playout>,
    }

    /// The `VoiceProcessingIO` unit: the microphone with the platform's echo cancellation,
    /// noise suppression and gain control, and the speaker or receiver.
    ///
    /// It never touches `AVAudioSession`: see the module documentation for the contract with
    /// the app.
    pub struct VoiceProcessingBackend {
        unit: AudioUnit,
        running: Option<Running>,
        capture_dropped: Counter,
        playout_underruns: Counter,
        capture_errors: Counter,
    }

    // SAFETY: the unit handle may be used from any thread; Core Audio serialises property
    // calls on its own. The callback state behind `running` is only reached by the unit's
    // real-time thread while it runs and by `stop`/`drop` after it stopped, both through
    // `&mut self`, so moving the owner to another thread shares nothing.
    unsafe impl Send for VoiceProcessingBackend {}

    impl VoiceProcessingBackend {
        /// Creates the unit, stopped. Fails if the system has no `VoiceProcessingIO`.
        pub fn new() -> Result<Self, AudioError> {
            let description = AudioComponentDescription {
                component_type: K_AUDIO_UNIT_TYPE_OUTPUT,
                component_sub_type: K_AUDIO_UNIT_SUB_TYPE_VOICE_PROCESSING_IO,
                component_manufacturer: K_AUDIO_UNIT_MANUFACTURER_APPLE,
                component_flags: 0,
                component_flags_mask: 0,
            };
            // SAFETY: a valid description; a null component starts the search from the first.
            let component = unsafe { AudioComponentFindNext(ptr::null_mut(), &description) };
            if component.is_null() {
                return Err(AudioError::NoDevice);
            }
            let mut unit: AudioUnit = ptr::null_mut();
            // SAFETY: `component` was just found and `unit` is a valid place for the instance.
            check(
                unsafe { AudioComponentInstanceNew(component, &mut unit) },
                "AudioComponentInstanceNew",
            )?;
            if unit.is_null() {
                return Err(AudioError::NoDevice);
            }
            Ok(Self {
                unit,
                running: None,
                capture_dropped: Counter::default(),
                playout_underruns: Counter::default(),
                capture_errors: Counter::default(),
            })
        }

        /// Captured samples lost because the engine did not read in time, since the last start.
        pub fn capture_dropped(&self) -> Counter {
            self.capture_dropped.clone()
        }

        /// Speaker callbacks that played silence, since the last start.
        pub fn playout_underruns(&self) -> Counter {
            self.playout_underruns.clone()
        }

        /// Input callbacks that failed to render the microphone, since the last start.
        pub fn capture_errors(&self) -> Counter {
            self.capture_errors.clone()
        }

        fn set<T>(&self, id: u32, scope: u32, element: u32, value: &T) -> Result<(), AudioError> {
            // SAFETY: `value` is a live `T` of the size given, the type the property takes.
            let status = unsafe {
                AudioUnitSetProperty(
                    self.unit,
                    id,
                    scope,
                    element,
                    (value as *const T).cast(),
                    size_of::<T>() as u32,
                )
            };
            check(status, "AudioUnitSetProperty")
        }

        fn get<T: Default>(&self, id: u32, scope: u32, element: u32) -> Result<T, AudioError> {
            let mut value = T::default();
            let mut size = size_of::<T>() as u32;
            // SAFETY: `value` has room for the `size` bytes of the property's type `T`.
            let status = unsafe {
                AudioUnitGetProperty(
                    self.unit,
                    id,
                    scope,
                    element,
                    (&mut value as *mut T).cast(),
                    &mut size,
                )
            };
            check(status, "AudioUnitGetProperty")?;
            Ok(value)
        }

        /// Sets the stream the app reads from the microphone and the one it plays.
        fn set_client_format(&self, kind: SampleKind) -> Result<(), AudioError> {
            let format = requested_format(kind);
            let stream = K_AUDIO_UNIT_PROPERTY_STREAM_FORMAT;
            self.set(stream, K_AUDIO_UNIT_SCOPE_OUTPUT, INPUT_BUS, &format)?;
            self.set(stream, K_AUDIO_UNIT_SCOPE_INPUT, OUTPUT_BUS, &format)
        }

        /// Everything that must be set before `AudioUnitInitialize`.
        fn configure(&self) -> Result<(), AudioError> {
            let enable_io = K_AUDIO_OUTPUT_UNIT_PROPERTY_ENABLE_IO;
            self.set(enable_io, K_AUDIO_UNIT_SCOPE_INPUT, INPUT_BUS, &1u32)?;
            self.set(enable_io, K_AUDIO_UNIT_SCOPE_OUTPUT, OUTPUT_BUS, &1u32)?;
            // The input callback renders into the capture's own buffer.
            let allocate = K_AUDIO_UNIT_PROPERTY_SHOULD_ALLOCATE_BUFFER;
            self.set(allocate, K_AUDIO_UNIT_SCOPE_OUTPUT, INPUT_BUS, &0u32)?;
            let max_frames = K_AUDIO_UNIT_PROPERTY_MAXIMUM_FRAMES_PER_SLICE;
            self.set(
                max_frames,
                K_AUDIO_UNIT_SCOPE_GLOBAL,
                0,
                &MAX_FRAMES_PER_SLICE,
            )?;
            // i16 is the engine's own format; f32 if the unit will not take it.
            self.set_client_format(SampleKind::I16)
                .or_else(|_| self.set_client_format(SampleKind::F32))
        }

        /// Reads back what the initialised unit gives, builds the callback state and starts.
        fn start_initialized(&mut self, io: DeviceIo) -> Result<(), AudioError> {
            let stream = K_AUDIO_UNIT_PROPERTY_STREAM_FORMAT;
            let capture_format =
                device_format(&self.get(stream, K_AUDIO_UNIT_SCOPE_OUTPUT, INPUT_BUS)?)?;
            let playout_format =
                device_format(&self.get(stream, K_AUDIO_UNIT_SCOPE_INPUT, OUTPUT_BUS)?)?;
            let max_frames: u32 = self.get(
                K_AUDIO_UNIT_PROPERTY_MAXIMUM_FRAMES_PER_SLICE,
                K_AUDIO_UNIT_SCOPE_GLOBAL,
                0,
            )?;

            let capture = Capture::new(capture_format, max_frames, io.capture)?;
            let playout = Playout::new(playout_format, io.playout)?;
            self.capture_dropped = capture.dropped();
            self.capture_errors = capture.errors();
            self.playout_underruns = playout.underruns();
            let mut input = Box::new(Input {
                unit: self.unit,
                capture,
            });
            let mut playout = Box::new(playout);

            let input_callback = AURenderCallbackStruct {
                input_proc: Some(input_callback),
                input_proc_ref_con: (&mut *input as *mut Input).cast(),
            };
            let render_callback = AURenderCallbackStruct {
                input_proc: Some(render_callback),
                input_proc_ref_con: (&mut *playout as *mut Playout).cast(),
            };
            self.set(
                K_AUDIO_OUTPUT_UNIT_PROPERTY_SET_INPUT_CALLBACK,
                K_AUDIO_UNIT_SCOPE_GLOBAL,
                INPUT_BUS,
                &input_callback,
            )?;
            self.set(
                K_AUDIO_UNIT_PROPERTY_SET_RENDER_CALLBACK,
                K_AUDIO_UNIT_SCOPE_INPUT,
                OUTPUT_BUS,
                &render_callback,
            )?;

            // From here the unit may call back, so the state is kept until it is stopped.
            self.running = Some(Running {
                _input: input,
                _playout: playout,
            });
            // SAFETY: an initialised unit whose callbacks point at the boxes in `running`.
            check(
                unsafe { AudioOutputUnitStart(self.unit) },
                "AudioOutputUnitStart",
            )
        }
    }

    impl AudioBackend for VoiceProcessingBackend {
        /// Starts the unit. The app calls it once the audio session is active: with CallKit,
        /// from `provider(_:didActivate:)`.
        fn start(&mut self, io: DeviceIo) -> Result<(), AudioError> {
            self.stop()?;
            self.configure()?;
            // SAFETY: a configured unit that is not initialised.
            check(
                unsafe { AudioUnitInitialize(self.unit) },
                "AudioUnitInitialize",
            )?;
            let started = self.start_initialized(io);
            if started.is_err() {
                // Leaves the unit uninitialised, and frees the state if it got that far.
                let _ = self.stop();
                // SAFETY: undoes the `AudioUnitInitialize` above; harmless if `stop` did.
                unsafe { AudioUnitUninitialize(self.unit) };
            }
            started
        }

        /// Stops and uninitialises the unit, then frees the callback state: nothing is freed
        /// on the audio thread. The app calls it on CallKit's `didDeactivate` or when the call
        /// ends.
        fn stop(&mut self) -> Result<(), AudioError> {
            let Some(running) = self.running.take() else {
                return Ok(());
            };
            // SAFETY: the unit is alive; stopping and uninitialising are valid in any state.
            let stopped = check(
                unsafe { AudioOutputUnitStop(self.unit) },
                "AudioOutputUnitStop",
            );
            // SAFETY: as above.
            let uninitialised = check(
                unsafe { AudioUnitUninitialize(self.unit) },
                "AudioUnitUninitialize",
            );
            if stopped.is_err() {
                // The unit may still call back: keep the state until `drop` disposes the unit.
                self.running = Some(running);
            }
            stopped.and(uninitialised)
        }
    }

    impl Drop for VoiceProcessingBackend {
        fn drop(&mut self) {
            let _ = self.stop();
            // SAFETY: the unit came from `AudioComponentInstanceNew` and is disposed once, here.
            // Once disposed it never calls back, so the state still in `running` after a failed
            // stop is freed safely when the fields drop, after this.
            unsafe { AudioComponentInstanceDispose(self.unit) };
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::FRAME_SAMPLES;
        use crate::audio::audio_io;
        use std::ffi::c_char;
        use std::time::Duration;

        type Id = *mut c_void;

        #[link(name = "objc")]
        unsafe extern "C" {
            fn objc_getClass(name: *const c_char) -> Id;
            fn sel_registerName(name: *const c_char) -> *mut c_void;
            fn objc_msgSend();
        }

        #[link(name = "AVFAudio", kind = "framework")]
        unsafe extern "C" {
            static AVAudioSessionCategoryPlayAndRecord: Id;
            static AVAudioSessionModeVoiceChat: Id;
        }

        /// Stands in for the app's Swift side, which the library leaves the session to:
        /// `playAndRecord` + `voiceChat`, active. Without it the unit initialises but will not
        /// start (OSStatus -66637).
        fn activate_voice_chat_session() {
            type SharedInstance = unsafe extern "C" fn(Id, *mut c_void) -> Id;
            type SetCategory =
                unsafe extern "C" fn(Id, *mut c_void, Id, Id, usize, *mut Id) -> bool;
            type SetActive = unsafe extern "C" fn(Id, *mut c_void, bool, *mut Id) -> bool;
            // SAFETY: each `objc_msgSend` is cast to the exact signature of the method it
            // sends, as arm64 requires; the class, selectors and constants exist on iOS 15+.
            unsafe {
                let send = objc_msgSend as *const ();
                let class = objc_getClass(c"AVAudioSession".as_ptr());
                let shared: SharedInstance = std::mem::transmute(send);
                let session = shared(class, sel_registerName(c"sharedInstance".as_ptr()));
                assert!(!session.is_null());
                let set_category: SetCategory = std::mem::transmute(send);
                let mut error: Id = std::ptr::null_mut();
                assert!(set_category(
                    session,
                    sel_registerName(c"setCategory:mode:options:error:".as_ptr()),
                    AVAudioSessionCategoryPlayAndRecord,
                    AVAudioSessionModeVoiceChat,
                    0,
                    &mut error,
                ));
                let set_active: SetActive = std::mem::transmute(send);
                assert!(set_active(
                    session,
                    sel_registerName(c"setActive:error:".as_ptr()),
                    true,
                    &mut error,
                ));
            }
        }

        #[test]
        fn the_backend_can_move_to_another_thread() {
            fn assert_send<T: Send>() {}
            assert_send::<VoiceProcessingBackend>();
        }

        // Creates the unit without starting it: no session or microphone needed.
        #[test]
        fn the_platform_backend_loads_on_ios() {
            let mut backend = crate::audio::platform_backend().unwrap();
            assert_eq!(backend.maintain(), Ok(()));
            assert_eq!(backend.stop(), Ok(()));
        }

        // Needs the real unit: run in the iOS simulator (see CLAUDE.md), which captures from
        // the Mac's microphone.
        #[test]
        #[ignore]
        fn the_unit_captures_and_plays_until_stopped() {
            let (device, mut engine) = audio_io(64);
            let queued = engine.playout.frames_free();
            for _ in 0..10 {
                assert!(engine.playout.write_frame(&[0; FRAME_SAMPLES]));
            }
            activate_voice_chat_session();
            let mut backend = VoiceProcessingBackend::new().unwrap();
            backend.start(device).unwrap();
            std::thread::sleep(Duration::from_millis(1000));
            backend.stop().unwrap();
            backend.stop().unwrap();

            let mut frames = 0;
            let mut frame = [0; FRAME_SAMPLES];
            while engine.capture.read_frame(&mut frame) {
                frames += 1;
            }
            // About 50 frames in a second; the unit takes a moment to start.
            assert!(frames >= 10, "captured {frames} frames");
            assert_eq!(
                engine.playout.frames_free(),
                queued,
                "playout was not drained"
            );
            assert!(backend.playout_underruns().get() > 0);
            assert_eq!(backend.capture_errors().get(), 0);
        }

        #[test]
        #[ignore]
        fn the_unit_starts_again_after_a_stop() {
            activate_voice_chat_session();
            let mut backend = VoiceProcessingBackend::new().unwrap();
            for _ in 0..2 {
                let (device, mut engine) = audio_io(64);
                backend.start(device).unwrap();
                std::thread::sleep(Duration::from_millis(500));
                backend.stop().unwrap();
                let mut frame = [0; FRAME_SAMPLES];
                assert!(engine.capture.read_frame(&mut frame));
            }
        }
    }
}

/// Turns the result of the Core Audio function `call` into a `Result`.
pub(crate) fn check(status: OSStatus, call: &'static str) -> Result<(), AudioError> {
    if status == 0 {
        return Ok(());
    }
    // Many Core Audio errors are four ASCII characters packed into the number, like '!pri'.
    let code = status.to_be_bytes();
    let reason = if code.iter().all(|byte| byte.is_ascii_graphic()) {
        let text: String = code.iter().map(|&byte| char::from(byte)).collect();
        format!("{call} failed: OSStatus {status} ('{text}')")
    } else {
        format!("{call} failed: OSStatus {status}")
    };
    Err(AudioError::Backend(reason))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::RingConsumer;
    use crate::audio::ring::ring;
    use std::mem::size_of_val;

    #[test]
    fn the_c_types_have_the_sdk_layout() {
        assert_eq!(size_of::<AudioStreamBasicDescription>(), 40);
        assert_eq!(size_of::<AudioBuffer>(), 16);
        assert_eq!(size_of::<AudioBufferList>(), 24);
        assert_eq!(size_of::<AudioComponentDescription>(), 20);
        assert_eq!(size_of::<AURenderCallbackStruct>(), 16);
    }

    #[test]
    fn asks_for_48000_mono_i16() {
        assert_eq!(
            requested_format(SampleKind::I16),
            AudioStreamBasicDescription {
                sample_rate: f64::from(SAMPLE_RATE),
                format_id: K_AUDIO_FORMAT_LINEAR_PCM,
                format_flags: K_AUDIO_FORMAT_FLAG_IS_SIGNED_INTEGER | K_AUDIO_FORMAT_FLAG_IS_PACKED,
                bytes_per_packet: 2,
                frames_per_packet: 1,
                bytes_per_frame: 2,
                channels_per_frame: 1,
                bits_per_channel: 16,
                reserved: 0,
            }
        );
    }

    #[test]
    fn or_48000_mono_f32() {
        let format = requested_format(SampleKind::F32);
        assert_eq!(
            format.format_flags,
            K_AUDIO_FORMAT_FLAG_IS_FLOAT | K_AUDIO_FORMAT_FLAG_IS_PACKED
        );
        assert_eq!(format.sample_rate, f64::from(SAMPLE_RATE));
        assert_eq!(format.channels_per_frame, 1);
        assert_eq!(format.bits_per_channel, 32);
        assert_eq!(format.bytes_per_frame, 4);
        assert_eq!(format.bytes_per_packet, 4);
        assert_eq!(format.frames_per_packet, 1);
    }

    fn format(sample_rate: u32, channels: u16, kind: SampleKind) -> DeviceFormat {
        DeviceFormat {
            stream: StreamFormat {
                sample_rate,
                channels,
            },
            kind,
        }
    }

    #[test]
    fn what_was_asked_for_reads_back_as_is() {
        assert_eq!(
            device_format(&requested_format(SampleKind::I16)),
            Ok(format(SAMPLE_RATE, 1, SampleKind::I16))
        );
        assert_eq!(
            device_format(&requested_format(SampleKind::F32)),
            Ok(format(SAMPLE_RATE, 1, SampleKind::F32))
        );
    }

    #[test]
    fn another_rate_and_interleaved_stereo_are_taken() {
        let mut description = requested_format(SampleKind::F32);
        description.sample_rate = 44_100.0;
        description.channels_per_frame = 2;
        description.bytes_per_frame = 8;
        description.bytes_per_packet = 8;
        assert_eq!(
            device_format(&description),
            Ok(format(44_100, 2, SampleKind::F32))
        );
    }

    #[test]
    fn mono_marked_non_interleaved_is_the_same_stream() {
        let mut description = requested_format(SampleKind::I16);
        description.format_flags |= K_AUDIO_FORMAT_FLAG_IS_NON_INTERLEAVED;
        assert_eq!(
            device_format(&description),
            Ok(format(SAMPLE_RATE, 1, SampleKind::I16))
        );
    }

    #[test]
    fn streams_the_adapters_cannot_take_are_rejected() {
        let i16_mono = requested_format(SampleKind::I16);
        let rejected = [
            // Non-interleaved stereo comes as two buffers.
            AudioStreamBasicDescription {
                format_flags: i16_mono.format_flags | K_AUDIO_FORMAT_FLAG_IS_NON_INTERLEAVED,
                channels_per_frame: 2,
                ..i16_mono
            },
            AudioStreamBasicDescription {
                format_flags: i16_mono.format_flags | K_AUDIO_FORMAT_FLAG_IS_BIG_ENDIAN,
                ..i16_mono
            },
            // 8.24 fixed point, the old canonical unit format.
            AudioStreamBasicDescription {
                bits_per_channel: 32,
                bytes_per_frame: 4,
                bytes_per_packet: 4,
                ..i16_mono
            },
            AudioStreamBasicDescription {
                format_id: u32::from_be_bytes(*b"aac "),
                ..i16_mono
            },
            AudioStreamBasicDescription {
                format_flags: K_AUDIO_FORMAT_FLAG_IS_PACKED,
                ..i16_mono
            },
            // Padded frames: the buffer arithmetic assumes packed samples.
            AudioStreamBasicDescription {
                bytes_per_frame: 4,
                bytes_per_packet: 4,
                ..i16_mono
            },
            AudioStreamBasicDescription {
                channels_per_frame: 70_000,
                bytes_per_frame: 140_000,
                bytes_per_packet: 140_000,
                ..i16_mono
            },
            AudioStreamBasicDescription {
                channels_per_frame: 0,
                ..i16_mono
            },
            AudioStreamBasicDescription {
                sample_rate: 0.0,
                ..i16_mono
            },
            AudioStreamBasicDescription {
                sample_rate: 44_100.5,
                ..i16_mono
            },
            AudioStreamBasicDescription {
                sample_rate: f64::NAN,
                ..i16_mono
            },
        ];
        for description in rejected {
            assert!(
                device_format(&description).is_err(),
                "accepted {description:?}"
            );
        }
    }

    fn drain(consumer: &mut RingConsumer) -> Vec<i16> {
        let mut all = vec![0; consumer.len()];
        consumer.pop(&mut all);
        all
    }

    /// What a render callback sees of the list it is handed.
    #[derive(Debug, PartialEq)]
    struct Seen {
        buffers: u32,
        channels: u32,
        bytes: u32,
    }

    fn seen(list: &AudioBufferList) -> Seen {
        Seen {
            buffers: list.number_buffers,
            channels: list.buffers[0].number_channels,
            bytes: list.buffers[0].data_byte_size,
        }
    }

    /// Stands in for `AudioUnitRender`: writes `samples` where the list points.
    fn write<S: Copy>(list: &mut AudioBufferList, samples: &[S]) {
        let buffer = &mut list.buffers[0];
        assert!(size_of_val(samples) <= buffer.data_byte_size as usize);
        // SAFETY: the list points at a buffer of `data_byte_size` bytes of `S`, checked above.
        let target =
            unsafe { std::slice::from_raw_parts_mut(buffer.data.cast::<S>(), samples.len()) };
        target.copy_from_slice(samples);
    }

    #[test]
    fn rendered_i16_samples_reach_the_ring_exactly() {
        let (producer, mut consumer) = ring(4096);
        let mut capture =
            Capture::new(format(SAMPLE_RATE, 1, SampleKind::I16), 512, producer).unwrap();
        let samples: Vec<i16> = (0..480).map(|n| n * 50 - 12000).collect();

        let mut seen_by_render = None;
        capture.process(480, |list| {
            seen_by_render = Some(seen(list));
            write(list, &samples);
            0
        });

        assert_eq!(
            seen_by_render,
            Some(Seen {
                buffers: 1,
                channels: 1,
                bytes: 960
            })
        );
        assert_eq!(drain(&mut consumer), samples);
        assert_eq!(capture.errors().get(), 0);
    }

    #[test]
    fn rendered_f32_stereo_is_mixed_to_mono_i16() {
        let (producer, mut consumer) = ring(4096);
        let mut capture =
            Capture::new(format(SAMPLE_RATE, 2, SampleKind::F32), 512, producer).unwrap();

        let mut seen_by_render = None;
        capture.process(2, |list| {
            seen_by_render = Some(seen(list));
            write(list, &[0.5f32, 0.5, -0.25, -0.25]);
            0
        });

        assert_eq!(
            seen_by_render,
            Some(Seen {
                buffers: 1,
                channels: 2,
                bytes: 16
            })
        );
        assert_eq!(drain(&mut consumer), [16384, -8192]);
    }

    #[test]
    fn a_failed_render_pushes_nothing_and_is_counted() {
        let (producer, mut consumer) = ring(4096);
        let mut capture =
            Capture::new(format(SAMPLE_RATE, 1, SampleKind::I16), 512, producer).unwrap();
        capture.process(256, |list| {
            write(list, &[1i16; 256]);
            -10863
        });
        assert!(drain(&mut consumer).is_empty());
        assert_eq!(capture.errors().get(), 1);
    }

    #[test]
    fn a_callback_larger_than_the_buffer_is_skipped_and_counted() {
        let (producer, mut consumer) = ring(4096);
        let mut capture =
            Capture::new(format(SAMPLE_RATE, 1, SampleKind::I16), 512, producer).unwrap();
        let mut rendered = false;
        capture.process(513, |_| {
            rendered = true;
            0
        });
        assert!(!rendered);
        assert!(drain(&mut consumer).is_empty());
        assert_eq!(capture.errors().get(), 1);

        capture.process(512, |list| {
            write(list, &[7i16; 512]);
            0
        });
        assert_eq!(drain(&mut consumer), [7; 512]);
    }

    #[test]
    fn a_render_that_points_the_list_elsewhere_is_counted_and_ignored() {
        let (producer, mut consumer) = ring(4096);
        let mut capture =
            Capture::new(format(SAMPLE_RATE, 1, SampleKind::I16), 512, producer).unwrap();
        let mut elsewhere = [5i16; 16];
        capture.process(16, |list| {
            list.buffers[0].data = elsewhere.as_mut_ptr().cast();
            0
        });
        assert!(drain(&mut consumer).is_empty());
        assert_eq!(capture.errors().get(), 1);
    }

    #[test]
    fn samples_that_find_the_ring_full_are_counted_as_dropped() {
        let (producer, _consumer) = ring(8);
        let mut capture =
            Capture::new(format(SAMPLE_RATE, 1, SampleKind::I16), 512, producer).unwrap();
        let dropped = capture.dropped();
        capture.process(10, |list| {
            write(list, &[1i16; 10]);
            0
        });
        assert_eq!(dropped.get(), 2);
    }

    #[test]
    fn only_the_bytes_the_render_reports_are_pushed() {
        let (producer, mut consumer) = ring(4096);
        let mut capture =
            Capture::new(format(SAMPLE_RATE, 1, SampleKind::I16), 512, producer).unwrap();
        capture.process(4, |list| {
            write(list, &[1i16, 2, 3, 4]);
            list.buffers[0].data_byte_size = 4;
            0
        });
        assert_eq!(drain(&mut consumer), [1, 2]);
    }

    /// A two-buffer `AudioBufferList`, as the unit would hand non-interleaved stereo.
    #[repr(C)]
    struct TwoBufferList {
        number_buffers: u32,
        buffers: [AudioBuffer; 2],
    }

    fn buffer_for<S>(samples: &mut [S], channels: u32) -> AudioBuffer {
        AudioBuffer {
            number_channels: channels,
            data_byte_size: size_of_val(samples) as u32,
            data: samples.as_mut_ptr().cast(),
        }
    }

    /// Runs the render callback the way the unit does.
    fn call_render(playout: &mut Playout, list: *mut AudioBufferList) -> OSStatus {
        let mut flags = 0;
        // SAFETY: `playout` is borrowed for the call alone; `list` is null or built by the test
        // over live buffers.
        unsafe {
            render_callback(
                (playout as *mut Playout).cast(),
                &mut flags,
                std::ptr::null(),
                0,
                0,
                list,
            )
        }
    }

    fn playout_with(format: DeviceFormat, queued: &[i16]) -> (Playout, crate::audio::RingProducer) {
        let (mut producer, consumer) = ring(4096);
        producer.push(queued);
        (Playout::new(format, consumer).unwrap(), producer)
    }

    #[test]
    fn the_render_callback_plays_the_ring_into_an_i16_buffer() {
        let queued: Vec<i16> = (0..256).map(|n| n * 100 - 12800).collect();
        let (mut playout, _producer) =
            playout_with(format(SAMPLE_RATE, 1, SampleKind::I16), &queued);
        let mut played = vec![0i16; 256];
        let mut list = AudioBufferList {
            number_buffers: 1,
            buffers: [buffer_for(&mut played, 1)],
        };

        assert_eq!(call_render(&mut playout, &mut list), 0);
        assert_eq!(played, queued);
        assert_eq!(playout.underruns().get(), 0);
    }

    #[test]
    fn the_render_callback_fills_f32_stereo_from_mono() {
        let (mut playout, _producer) =
            playout_with(format(SAMPLE_RATE, 2, SampleKind::F32), &[16384, -8192]);
        let mut played = [9.0f32; 4];
        let mut list = AudioBufferList {
            number_buffers: 1,
            buffers: [buffer_for(&mut played, 2)],
        };

        assert_eq!(call_render(&mut playout, &mut list), 0);
        assert_eq!(played, [0.5, 0.5, -0.25, -0.25]);
    }

    #[test]
    fn a_dry_ring_plays_silence_and_counts_an_underrun() {
        let (mut playout, _producer) = playout_with(format(SAMPLE_RATE, 1, SampleKind::I16), &[]);
        let mut played = [7i16; 64];
        let mut list = AudioBufferList {
            number_buffers: 1,
            buffers: [buffer_for(&mut played, 1)],
        };

        assert_eq!(call_render(&mut playout, &mut list), 0);
        assert_eq!(played, [0; 64]);
        assert_eq!(playout.underruns().get(), 1);
    }

    #[test]
    fn buffers_after_the_first_get_silence() {
        let (mut playout, _producer) =
            playout_with(format(SAMPLE_RATE, 1, SampleKind::I16), &[1, 2, 3, 4]);
        let mut first = [9i16; 4];
        let mut second = [9i16; 4];
        let mut list = TwoBufferList {
            number_buffers: 2,
            buffers: [buffer_for(&mut first, 1), buffer_for(&mut second, 1)],
        };

        let status = call_render(&mut playout, (&mut list as *mut TwoBufferList).cast());
        assert_eq!(status, 0);
        assert_eq!(first, [1, 2, 3, 4]);
        assert_eq!(second, [0; 4]);
    }

    #[test]
    fn no_buffer_list_is_a_parameter_error() {
        let (mut playout, _producer) = playout_with(format(SAMPLE_RATE, 1, SampleKind::I16), &[]);
        assert_eq!(
            call_render(&mut playout, std::ptr::null_mut()),
            K_AUDIO_PARAM_ERROR
        );
    }

    #[test]
    fn a_zero_status_is_success() {
        assert_eq!(check(0, "AudioUnitInitialize"), Ok(()));
    }

    #[test]
    fn a_failing_status_names_the_call_and_the_code() {
        let error = check(-10868, "AudioUnitSetProperty").unwrap_err();
        assert_eq!(
            error,
            AudioError::Backend("AudioUnitSetProperty failed: OSStatus -10868".to_owned())
        );
    }

    #[test]
    fn a_four_char_status_is_also_shown_as_text() {
        // '!pri': the session refused to activate or to record (AVAudioSession.ErrorCode).
        let error = check(0x2170_7269, "AudioOutputUnitStart").unwrap_err();
        assert_eq!(
            error,
            AudioError::Backend(
                "AudioOutputUnitStart failed: OSStatus 561017449 ('!pri')".to_owned()
            )
        );
    }
}
