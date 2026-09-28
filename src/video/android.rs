//! Android video: [`CameraSource`] (Camera2 + MediaCodec encoder) and [`DisplaySink`]
//! (MediaCodec decoder onto a surface). Android only; the pure parts are tested on the host.
//!
//! # Capture and encode
//!
//! The camera writes straight into the H.264 encoder's input surface
//! (`AMediaCodec_createInputSurface`): no pixel ever passes through the CPU. The encoder is
//! asked for Baseline at level 3.1, CBR (VBR, then no profile, if refused), a keyframe every two
//! seconds and real-time priority. A drain thread takes each output buffer, keeps the SPS and PPS
//! of the codec-config buffer and puts them in front of every keyframe (MediaCodec already emits
//! Annex-B; AVCC would be converted), and pushes an [`EncodedFrame`] into the
//! [`FrameSender`](super::FrameSender), with the rotation from the sensor orientation, the
//! facing and the app's screen rotation. Pixels are neither rotated nor mirrored.
//! `set_bitrate` and `request_keyframe` go through `AMediaCodec_setParameters`
//! (`video-bitrate`, `request-sync`); the drain thread also asks for a keyframe when the channel
//! needs one. A camera switch keeps the encoder when the new camera can capture at its size
//! (the size is picked among those both cameras share) and asks for a keyframe.
//!
//! # Decode and render
//!
//! The decoder is configured at the first keyframe from its SPS and PPS (`csd-0`, `csd-1`, the
//! size read from the SPS) and renders every frame to the app's surface as soon as it is decoded
//! (`releaseOutputBuffer(render = true)`), from a thread of its own. The rotation is applied by
//! the decoder (`rotation-degrees`), so a plain `SurfaceView` shows the video upright; a new
//! rotation needs a new decoder, so it waits for the next keyframe and, meanwhile, the sink asks
//! for one. [`DisplaySink::video_size`] gives the upright size for the view's aspect ratio.
//!
//! **Keyframe signal**: [`DisplaySink::keyframe_request`] gives a [`KeyframeRequest`] the
//! pipeline polls; while [`KeyframeRequest::is_needed`] is `true` it sends PLIs (rate limited on
//! its side). It turns on before the first keyframe, after a frame is lost (no free input
//! buffer), after a decoder error (the decoder is rebuilt at the next keyframe), after a surface
//! change the decoder could not follow, and while a rotation change waits; the next keyframe
//! turns it off. Decoder errors never fail [`VideoSink::push`](super::VideoSink::push).
//!
//! # Android versions
//!
//! `libmediandk.so` and `libcamera2ndk.so` are looked up at run time (`dlopen`), not linked, so
//! the native library still loads on the app's minimum, Android 7 (API 24).
//!
//! - [`CameraSource`] needs **Android 8.0 (API 26)**: the encoder's input surface and
//!   `setParameters` (bitrate, keyframes) are API 26. Before it, `start` returns
//!   [`VideoError::Unsupported`]. A copy path (`AImageReader` YUV into input buffers) was left
//!   out: on API 24–25 there would be no bitrate control and no keyframe on demand either.
//! - [`DisplaySink`] works from API 24. From API 26, while the app has no surface the decoder
//!   renders to a placeholder `AImageReader` and moves between surfaces without a reset
//!   (`setOutputSurface`); on API 24–25 a surface change rebuilds it at the next keyframe.
//! - `prepend-sps-pps-to-idr-frames` (API 29) and `low-latency` (API 30) are asked for and
//!   ignored before.
//!
//! # The app's side (Kotlin)
//!
//! The backend does not touch the Android framework; the app's Kotlin code:
//!
//! 1. holds the `CAMERA` permission before `CameraSource::start`. Without it (or with the
//!    camera disabled by policy) `start` returns [`VideoError::PermissionDenied`];
//! 2. hands each `Surface` over JNI, where the app's Rust side calls `ANativeWindow_fromSurface`
//!    and passes the window to [`CameraSource::set_preview_surface`] (local preview) or
//!    [`DisplaySink::set_surface`] (remote video). Both take a reference of their own, so the
//!    JNI side can `ANativeWindow_release` its own right away. In `surfaceDestroyed` it passes
//!    `None` before returning;
//! 3. uses `SurfaceView`s: the preview gets the camera's own transform (upright and mirrored for
//!    the front camera), and the remote video the decoder's rotation; nothing else to do;
//! 4. calls [`CameraSource::set_display_rotation`] when the activity turns (not needed for a
//!    portrait-only screen), and restarts the source if [`CameraSource::camera_lost`] (another
//!    app took the camera);
//! 5. for video in the background, runs the foreground service of type `camera`.
//!
//! The process needs binder threads for the camera to fill surfaces whose queue lives in it (the
//! app always has them; the device tests start them).
//!
//! # Real time
//!
//! No codec work runs in a platform callback: each codec has its own drain thread with a 10 ms
//! dequeue timeout (the async callbacks are API 28). The camera callbacks only set atomics
//! (device lost) or signal a condition (session closed).

use std::ffi::CStr;

use super::{EncodedFrame, Facing, Rotation, VideoConfig, VideoError, clamp_bitrate};

/// NAL unit types (H.264 table 7-1) the backend looks at.
const NAL_IDR: u8 = 5;
const NAL_SPS: u8 = 7;
const NAL_PPS: u8 = 8;
/// The 4-byte Annex-B start code.
const START_CODE: [u8; 4] = [0, 0, 0, 1];

/// `BUFFER_FLAG_KEY_FRAME`: the buffer holds a sync frame.
const BUFFER_FLAG_KEY_FRAME: u32 = 1;
/// `BUFFER_FLAG_CODEC_CONFIG`: the buffer holds codec-specific data (SPS and PPS), not a frame.
/// The packager recognises those by their NAL types, so only the tests use it.
#[cfg(test)]
const BUFFER_FLAG_CODEC_CONFIG: u32 = 2;

/// The type of a NAL unit (its first byte, without start code).
fn nal_type(nal: &[u8]) -> Option<u8> {
    nal.first().map(|header| header & 0x1f)
}

/// The NAL units of an Annex-B buffer, without their start codes. Bytes before the first start
/// code are skipped, and the zeros ahead of a 4-byte start code are not part of the unit before.
fn nal_units(data: &[u8]) -> Vec<&[u8]> {
    // Where each unit starts: just after a `00 00 01`.
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i..i + 3] == [0, 0, 1] {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    starts
        .iter()
        .enumerate()
        .filter_map(|(n, &start)| {
            let end = starts.get(n + 1).map_or(data.len(), |next| next - 3);
            let mut unit = data.get(start..end)?;
            while let [rest @ .., 0] = unit {
                unit = rest;
            }
            (!unit.is_empty()).then_some(unit)
        })
        .collect()
}

/// Whether `data` starts with an Annex-B start code.
fn is_annex_b(data: &[u8]) -> bool {
    data.starts_with(&[0, 0, 1]) || data.starts_with(&START_CODE)
}

/// `data` in Annex-B: as it is if it already is, converted if it is AVCC (4-byte lengths).
/// `None` if it is neither.
fn to_annex_b(data: &[u8]) -> Option<Vec<u8>> {
    if is_annex_b(data) {
        return Some(data.to_vec());
    }
    let mut out = Vec::with_capacity(data.len());
    let mut rest = data;
    while !rest.is_empty() {
        let (length, tail) = rest.split_first_chunk::<4>()?;
        let length = usize::try_from(u32::from_be_bytes(*length)).ok()?;
        if length == 0 || length > tail.len() {
            return None;
        }
        out.extend_from_slice(&START_CODE);
        out.extend_from_slice(&tail[..length]);
        rest = &tail[length..];
    }
    (!out.is_empty()).then_some(out)
}

/// Turns the encoder's output buffers into access units for the engine: keeps the last SPS and
/// PPS the encoder gave (in a codec-config buffer, or in-band) and puts them in front of every
/// keyframe that lacks them.
#[derive(Debug, Default)]
struct AccessUnitPackager {
    /// The latest SPS and PPS, in Annex-B.
    parameter_sets: Vec<u8>,
}

impl AccessUnitPackager {
    /// One output buffer with its `BUFFER_FLAG_*` flags: the access unit to send and whether
    /// it is a keyframe, or `None` for a codec-config buffer or one that holds no frame.
    fn package(&mut self, data: &[u8], flags: u32) -> Option<(Vec<u8>, bool)> {
        let data = to_annex_b(data)?;
        let units = nal_units(&data);
        let types: Vec<u8> = units.iter().filter_map(|unit| nal_type(unit)).collect();
        let has_parameter_sets = types.contains(&NAL_SPS);
        if has_parameter_sets {
            self.parameter_sets.clear();
            for unit in units
                .iter()
                .filter(|unit| matches!(nal_type(unit), Some(NAL_SPS | NAL_PPS)))
            {
                self.parameter_sets.extend_from_slice(&START_CODE);
                self.parameter_sets.extend_from_slice(unit);
            }
        }
        // Slices (types 1 to 5): anything else is not a frame.
        let has_frame = types.iter().any(|kind| (1..=NAL_IDR).contains(kind));
        if !has_frame {
            return None;
        }
        let keyframe = types.contains(&NAL_IDR) || flags & BUFFER_FLAG_KEY_FRAME != 0;
        if keyframe && !has_parameter_sets && !self.parameter_sets.is_empty() {
            let mut prefixed = Vec::with_capacity(self.parameter_sets.len() + data.len());
            prefixed.extend_from_slice(&self.parameter_sets);
            prefixed.extend_from_slice(&data);
            return Some((prefixed, true));
        }
        Some((data, keyframe))
    }
}

/// Reads the bits of an RBSP (emulation prevention bytes already removed), as H.264 7.2.
struct BitReader<'a> {
    data: &'a [u8],
    position: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, position: 0 }
    }

    fn bit(&mut self) -> Option<u32> {
        let byte = self.data.get(self.position / 8)?;
        let bit = (byte >> (7 - self.position % 8)) & 1;
        self.position += 1;
        Some(u32::from(bit))
    }

    fn bits(&mut self, count: u32) -> Option<u32> {
        (0..count).try_fold(0, |value, _| Some((value << 1) | self.bit()?))
    }

    /// `ue(v)`: unsigned Exp-Golomb.
    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        let rest = self.bits(zeros)?;
        (1u32 << zeros).checked_sub(1)?.checked_add(rest)
    }

    /// `se(v)`: signed Exp-Golomb.
    fn se(&mut self) -> Option<i64> {
        let code = i64::from(self.ue()?);
        Some(if code % 2 == 1 {
            (code + 1) / 2
        } else {
            -code / 2
        })
    }
}

/// A NAL unit's payload with the emulation prevention bytes (`00 00 03`) taken out.
fn unescape(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for &byte in nal {
        if zeros >= 2 && byte == 3 {
            zeros = 0;
            continue;
        }
        zeros = if byte == 0 { zeros + 1 } else { 0 };
        out.push(byte);
    }
    out
}

/// Skips a `scaling_list()` of `size` entries (H.264 7.3.2.1.1.1).
fn skip_scaling_list(reader: &mut BitReader<'_>, size: usize) -> Option<()> {
    let (mut last, mut next) = (8i64, 8i64);
    for _ in 0..size {
        if next != 0 {
            next = (last + reader.se()? + 256).rem_euclid(256);
        }
        if next != 0 {
            last = next;
        }
    }
    Some(())
}

/// The picture size an SPS (the NAL unit, header byte included) describes, after cropping.
fn sps_dimensions(sps: &[u8]) -> Option<(u32, u32)> {
    if nal_type(sps)? != NAL_SPS {
        return None;
    }
    let rbsp = unescape(sps.get(1..)?);
    let mut reader = BitReader::new(&rbsp);
    let profile_idc = reader.bits(8)?;
    reader.bits(16)?; // constraint flags, level_idc
    reader.ue()?; // seq_parameter_set_id
    let mut chroma_format_idc = 1;
    let mut separate_colour_plane = false;
    if matches!(
        profile_idc,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        chroma_format_idc = reader.ue()?;
        if chroma_format_idc == 3 {
            separate_colour_plane = reader.bit()? == 1;
        }
        reader.ue()?; // bit_depth_luma_minus8
        reader.ue()?; // bit_depth_chroma_minus8
        reader.bit()?; // qpprime_y_zero_transform_bypass_flag
        if reader.bit()? == 1 {
            let lists = if chroma_format_idc == 3 { 12 } else { 8 };
            for list in 0..lists {
                if reader.bit()? == 1 {
                    skip_scaling_list(&mut reader, if list < 6 { 16 } else { 64 })?;
                }
            }
        }
    }
    reader.ue()?; // log2_max_frame_num_minus4
    match reader.ue()? {
        0 => {
            reader.ue()?; // log2_max_pic_order_cnt_lsb_minus4
        }
        1 => {
            reader.bit()?; // delta_pic_order_always_zero_flag
            reader.se()?; // offset_for_non_ref_pic
            reader.se()?; // offset_for_top_to_bottom_field
            for _ in 0..reader.ue()? {
                reader.se()?; // offset_for_ref_frame
            }
        }
        _ => {}
    }
    reader.ue()?; // max_num_ref_frames
    reader.bit()?; // gaps_in_frame_num_value_allowed_flag
    let width_in_mbs = reader.ue()?.checked_add(1)?;
    let height_in_map_units = reader.ue()?.checked_add(1)?;
    let frame_mbs_only = reader.bit()?;
    if frame_mbs_only == 0 {
        reader.bit()?; // mb_adaptive_frame_field_flag
    }
    reader.bit()?; // direct_8x8_inference_flag
    let (mut left, mut right, mut top, mut bottom) = (0, 0, 0, 0);
    if reader.bit()? == 1 {
        left = reader.ue()?;
        right = reader.ue()?;
        top = reader.ue()?;
        bottom = reader.ue()?;
    }
    let field_factor = 2 - frame_mbs_only;
    let width = width_in_mbs.checked_mul(16)?;
    let height = height_in_map_units
        .checked_mul(16)?
        .checked_mul(field_factor)?;
    // Crop units (H.264 7-19 to 7-22): in chroma samples, unless there is no chroma array.
    let chroma_array_type = if separate_colour_plane {
        0
    } else {
        chroma_format_idc
    };
    let (crop_x, crop_y) = match chroma_array_type {
        0 => (1, field_factor),
        1 => (2, 2 * field_factor),
        2 => (2, field_factor),
        _ => (1, field_factor),
    };
    let width = width.checked_sub(left.checked_add(right)?.checked_mul(crop_x)?)?;
    let height = height.checked_sub(top.checked_add(bottom)?.checked_mul(crop_y)?)?;
    (width > 0 && height > 0).then_some((width, height))
}

/// What a decoder is configured with: taken from a keyframe.
#[derive(Debug, Clone, PartialEq, Eq)]
struct StreamParams {
    /// The SPS NAL unit, without start code.
    sps: Vec<u8>,
    /// The PPS NAL unit, without start code.
    pps: Vec<u8>,
    width: u32,
    height: u32,
    rotation: Rotation,
}

impl StreamParams {
    /// The size the video shows at, once rotated upright.
    fn displayed_size(&self) -> (u32, u32) {
        match self.rotation {
            Rotation::Deg90 | Rotation::Deg270 => (self.height, self.width),
            Rotation::Deg0 | Rotation::Deg180 => (self.width, self.height),
        }
    }

    /// The parameters of a keyframe: its first SPS and PPS, and the size the SPS gives.
    fn from_keyframe(frame: &EncodedFrame) -> Option<Self> {
        if !frame.keyframe {
            return None;
        }
        let units = nal_units(&frame.data);
        let find = |kind| {
            units
                .iter()
                .find(|unit| nal_type(unit) == Some(kind))
                .map(|unit| unit.to_vec())
        };
        let sps = find(NAL_SPS)?;
        let pps = find(NAL_PPS)?;
        let (width, height) = sps_dimensions(&sps)?;
        Some(Self {
            sps,
            pps,
            width,
            height,
            rotation: frame.rotation,
        })
    }
}

/// What the sink does with a frame, decided by [`SinkState::on_frame`].
#[derive(Debug, Clone, PartialEq, Eq)]
enum SinkStep {
    /// Not decodable now: wait for a keyframe.
    Drop,
    /// Queue it to the running decoder.
    Decode,
    /// (Re)build the decoder with these parameters, then queue the frame.
    Configure(StreamParams),
}

/// The sink's decisions, without the codec: which frames to drop until a keyframe, when to
/// (re)configure the decoder, and when to ask the sender for a keyframe.
#[derive(Debug)]
struct SinkState {
    /// What the running decoder was configured with; `None` without a decoder.
    configured: Option<StreamParams>,
    /// A frame was lost or the decoder failed: delta frames are dropped until a keyframe.
    waiting: bool,
    /// The sender turned: the new rotation takes effect at the next keyframe.
    rotation_pending: bool,
}

impl Default for SinkState {
    fn default() -> Self {
        Self {
            configured: None,
            waiting: true,
            rotation_pending: false,
        }
    }
}

impl SinkState {
    fn on_frame(&mut self, frame: &EncodedFrame) -> SinkStep {
        if frame.keyframe {
            let Some(params) = StreamParams::from_keyframe(frame) else {
                return SinkStep::Drop;
            };
            self.waiting = false;
            self.rotation_pending = false;
            if self.configured.as_ref() == Some(&params) {
                return SinkStep::Decode;
            }
            self.configured = Some(params.clone());
            return SinkStep::Configure(params);
        }
        match &self.configured {
            Some(configured) if !self.waiting => {
                self.rotation_pending = frame.rotation != configured.rotation;
                SinkStep::Decode
            }
            _ => SinkStep::Drop,
        }
    }

    /// A frame could not be queued: the following delta frames would decode against it.
    fn on_frame_lost(&mut self) {
        self.waiting = true;
    }

    /// The decoder failed or had to go: the next keyframe builds a new one.
    fn on_decoder_lost(&mut self) {
        self.waiting = true;
        self.configured = None;
    }

    /// Whether the sender should be asked for a keyframe (a PLI).
    fn keyframe_needed(&self) -> bool {
        self.waiting || self.rotation_pending
    }
}

/// How the receiver rotates a frame from a camera mounted at `sensor_orientation` degrees
/// (`SENSOR_ORIENTATION`), facing `facing`, while the screen is turned by `display` (the
/// `Display.getRotation()` of the app). The frame is neither rotated nor mirrored.
fn frame_rotation(sensor_orientation: i32, facing: Facing, display: Rotation) -> Rotation {
    // In quarter turns, the sensor's rounded to the nearest one.
    let sensor = (sensor_orientation.rem_euclid(360) + 45) / 90;
    let display = i32::from(display.to_cvo());
    let quarters = match facing {
        Facing::Front => sensor + display,
        Facing::Back => sensor - display,
    };
    Rotation::from_cvo(u8::try_from(quarters.rem_euclid(4)).unwrap_or(0))
}

/// `ACAMERA_LENS_FACING` to a [`Facing`]; external cameras are not picked.
fn lens_facing(value: u8) -> Option<Facing> {
    match value {
        0 => Some(Facing::Front),
        1 => Some(Facing::Back),
        _ => None,
    }
}

/// The output sizes for `format` in `ACAMERA_SCALER_AVAILABLE_STREAM_CONFIGURATIONS`
/// (`format, width, height, is_input` quadruples).
fn output_sizes(configurations: &[i32], format: i32) -> Vec<(u32, u32)> {
    configurations
        .chunks_exact(4)
        .filter(|entry| entry[0] == format && entry[3] == 0)
        .filter_map(|entry| {
            let width = u32::try_from(entry[1]).ok().filter(|&w| w > 0)?;
            let height = u32::try_from(entry[2]).ok().filter(|&h| h > 0)?;
            Some((width, height))
        })
        .collect()
}

/// The sizes in both lists, in the order of the first.
fn common_sizes(first: &[(u32, u32)], second: &[(u32, u32)]) -> Vec<(u32, u32)> {
    first
        .iter()
        .filter(|size| second.contains(size))
        .copied()
        .collect()
}

/// The size to capture: the one asked for if the camera has it; else the closest in area among
/// those with the same aspect ratio; else the closest in area.
fn choose_size(sizes: &[(u32, u32)], wanted: (u32, u32)) -> Option<(u32, u32)> {
    if sizes.contains(&wanted) {
        return Some(wanted);
    }
    let area = |(width, height): (u32, u32)| u64::from(width) * u64::from(height);
    // Within 1 %: w1 / h1 against w2 / h2, cross-multiplied.
    let same_shape = |(width, height): (u32, u32)| {
        let a = u64::from(width) * u64::from(wanted.1);
        let b = u64::from(wanted.0) * u64::from(height);
        a.abs_diff(b) * 100 <= b
    };
    let closest = |candidates: &mut dyn Iterator<Item = (u32, u32)>| {
        candidates.min_by_key(|&size| {
            (
                area(size).abs_diff(area(wanted)),
                std::cmp::Reverse(area(size)),
            )
        })
    };
    closest(&mut sizes.iter().copied().filter(|&size| same_shape(size)))
        .or_else(|| closest(&mut sizes.iter().copied()))
}

/// The auto-exposure frame rate range to ask for (`[min, max]` from
/// `ACAMERA_CONTROL_AE_AVAILABLE_TARGET_FPS_RANGES`): the top as close to `fps` as there is, and
/// the bottom near 15 fps, so exposure can stretch in a dim room without the video stuttering.
fn choose_fps_range(ranges: &[i32], fps: u32) -> Option<[i32; 2]> {
    let fps = i64::from(fps);
    let floor = fps.min(15);
    ranges
        .chunks_exact(2)
        .map(|range| [range[0], range[1]])
        .min_by_key(|&[min, max]| {
            // Some legacy HALs give the rates in thousandths.
            let scale = if max > 1000 { 1000 } else { 1 };
            let (min, max) = (i64::from(min / scale), i64::from(max / scale));
            (max - fps).abs() * 100 + (min - floor).abs()
        })
}

/// A `camera_status_t` error as a [`VideoError`].
fn camera_error(status: i32) -> VideoError {
    // `ACAMERA_ERROR_*` (`<camera/NdkCameraError.h>`).
    match status {
        // No `CAMERA` permission, or a device policy that disables the camera.
        -10013 | -10012 => VideoError::PermissionDenied,
        -10010 | -10011 => VideoError::Backend("camera in use by another app".to_owned()),
        other => VideoError::Backend(format!("camera error {other}")),
    }
}

/// A value in an `AMediaFormat`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum FormatValue {
    Int(i32),
    Str(&'static CStr),
    Buffer(Vec<u8>),
}

type FormatEntry = (&'static CStr, FormatValue);

const MIME_AVC: &CStr = c"video/avc";
const KEY_MIME: &CStr = c"mime";
const KEY_WIDTH: &CStr = c"width";
const KEY_HEIGHT: &CStr = c"height";
const KEY_COLOR_FORMAT: &CStr = c"color-format";
const KEY_BIT_RATE: &CStr = c"bitrate";
const KEY_BITRATE_MODE: &CStr = c"bitrate-mode";
const KEY_FRAME_RATE: &CStr = c"frame-rate";
const KEY_I_FRAME_INTERVAL: &CStr = c"i-frame-interval";
const KEY_PROFILE: &CStr = c"profile";
const KEY_LEVEL: &CStr = c"level";
const KEY_PRIORITY: &CStr = c"priority";
const KEY_PREPEND_HEADER_TO_SYNC_FRAMES: &CStr = c"prepend-sps-pps-to-idr-frames";
const KEY_ROTATION: &CStr = c"rotation-degrees";
const KEY_LOW_LATENCY: &CStr = c"low-latency";
const KEY_CSD_0: &CStr = c"csd-0";
const KEY_CSD_1: &CStr = c"csd-1";
const PARAMETER_VIDEO_BITRATE: &CStr = c"video-bitrate";
const PARAMETER_REQUEST_SYNC_FRAME: &CStr = c"request-sync";

/// `COLOR_FormatSurface`: the encoder takes its frames from its input surface.
const COLOR_FORMAT_SURFACE: i32 = 0x7F00_0789;
/// `BITRATE_MODE_VBR` and `BITRATE_MODE_CBR`.
const BITRATE_MODE_VBR: i32 = 1;
const BITRATE_MODE_CBR: i32 = 2;
/// `AVCProfileBaseline` and `AVCLevel31` (`42e01f` is Constrained Baseline at level 3.1).
const AVC_PROFILE_BASELINE: i32 = 1;
const AVC_LEVEL_31: i32 = 0x200;
/// Seconds between keyframes when nothing asks for one sooner.
const I_FRAME_INTERVAL_SECONDS: i32 = 2;

/// The encoder formats to try, best first: CBR (the rate the network allows, not more), then
/// VBR for encoders that refuse CBR, then without profile and level for encoders that refuse
/// those. Input from a surface; `width`×`height` is the capture size.
fn encoder_formats(width: u32, height: u32, config: &VideoConfig) -> Vec<Vec<FormatEntry>> {
    let int = |value: u32| FormatValue::Int(i32::try_from(value).unwrap_or(i32::MAX));
    let base = vec![
        (KEY_MIME, FormatValue::Str(MIME_AVC)),
        (KEY_WIDTH, int(width)),
        (KEY_HEIGHT, int(height)),
        (KEY_COLOR_FORMAT, FormatValue::Int(COLOR_FORMAT_SURFACE)),
        (KEY_BIT_RATE, int(clamp_bitrate(config.bitrate_bps))),
        (KEY_FRAME_RATE, int(config.fps)),
        (
            KEY_I_FRAME_INTERVAL,
            FormatValue::Int(I_FRAME_INTERVAL_SECONDS),
        ),
        // 0 = real time.
        (KEY_PRIORITY, FormatValue::Int(0)),
        // Android 10+; older encoders ignore it, and the packager adds them anyway.
        (KEY_PREPEND_HEADER_TO_SYNC_FRAMES, FormatValue::Int(1)),
    ];
    let with = |mode: i32, profile: bool| {
        let mut format = base.clone();
        format.push((KEY_BITRATE_MODE, FormatValue::Int(mode)));
        if profile {
            format.push((KEY_PROFILE, FormatValue::Int(AVC_PROFILE_BASELINE)));
            format.push((KEY_LEVEL, FormatValue::Int(AVC_LEVEL_31)));
        }
        format
    };
    vec![
        with(BITRATE_MODE_CBR, true),
        with(BITRATE_MODE_VBR, true),
        with(BITRATE_MODE_VBR, false),
    ]
}

/// The decoder format for `params`: `csd-0` and `csd-1` hold the SPS and PPS in Annex-B, and
/// the rotation is applied by the decoder when it renders to the surface.
fn decoder_format(params: &StreamParams) -> Vec<FormatEntry> {
    let int = |value: u32| FormatValue::Int(i32::try_from(value).unwrap_or(i32::MAX));
    let annex_b = |nal: &[u8]| FormatValue::Buffer([&START_CODE[..], nal].concat());
    vec![
        (KEY_MIME, FormatValue::Str(MIME_AVC)),
        (KEY_WIDTH, int(params.width)),
        (KEY_HEIGHT, int(params.height)),
        (
            KEY_ROTATION,
            FormatValue::Int(i32::from(params.rotation.degrees())),
        ),
        (KEY_PRIORITY, FormatValue::Int(0)),
        // Android 11+: output each frame as soon as it is decoded.
        (KEY_LOW_LATENCY, FormatValue::Int(1)),
        (KEY_CSD_0, annex_b(&params.sps)),
        (KEY_CSD_1, annex_b(&params.pps)),
    ]
}

/// `AMediaCodec_setParameters` for a new target bitrate, clamped.
fn bitrate_parameters(bps: u32) -> Vec<FormatEntry> {
    let bps = i32::try_from(clamp_bitrate(bps)).unwrap_or(i32::MAX);
    vec![(PARAMETER_VIDEO_BITRATE, FormatValue::Int(bps))]
}

/// `AMediaCodec_setParameters` to make the next frame a keyframe.
fn keyframe_parameters() -> Vec<FormatEntry> {
    vec![(PARAMETER_REQUEST_SYNC_FRAME, FormatValue::Int(0))]
}

/// Frames the encoder is given to honour a keyframe request before it is asked again.
const KEYFRAME_RETRY_FRAMES: u32 = 30;

/// When the encoder's drain thread asks for a keyframe: once when the channel needs one, and
/// again only if none came out within [`KEYFRAME_RETRY_FRAMES`] frames.
#[derive(Debug, Default)]
struct KeyframeRequests {
    /// Frames out since the pending request; `None` with nothing pending.
    pending: Option<u32>,
}

impl KeyframeRequests {
    /// Notes a request sent from elsewhere (the engine's `request_keyframe`).
    fn requested(&mut self) {
        self.pending = Some(0);
    }

    /// After each encoded frame: whether to ask the encoder for a keyframe now.
    fn after_frame(&mut self, keyframe: bool, needed: bool) -> bool {
        if keyframe {
            self.pending = None;
            return false;
        }
        if !needed {
            return false;
        }
        match self.pending {
            Some(frames) if frames + 1 < KEYFRAME_RETRY_FRAMES => {
                self.pending = Some(frames + 1);
                false
            }
            _ => {
                self.pending = Some(0);
                true
            }
        }
    }
}

#[cfg(target_os = "android")]
pub use device::{ANativeWindow, CameraSource, DisplaySink, KeyframeRequest};

#[cfg(target_os = "android")]
mod device {
    use std::ffi::{CString, c_char, c_int, c_void};
    use std::ptr::{self, NonNull};
    use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU8, Ordering};
    use std::sync::{Arc, Condvar, Mutex, OnceLock};
    use std::thread::{self, JoinHandle};
    use std::time::Duration;

    use super::*;
    use crate::video::{FrameSender, VideoSink, VideoSource};

    /// `ANativeWindow`, opaque: the native side of a Java `Surface`
    /// (`ANativeWindow_fromSurface`). The same type as `ndk_sys::ANativeWindow`; cast the pointer.
    #[repr(C)]
    pub struct ANativeWindow {
        _private: [u8; 0],
    }

    #[link(name = "android")]
    unsafe extern "C" {
        fn ANativeWindow_acquire(window: *mut ANativeWindow);
        fn ANativeWindow_release(window: *mut ANativeWindow);
        fn ANativeWindow_setBuffersGeometry(
            window: *mut ANativeWindow,
            width: i32,
            height: i32,
            format: i32,
        ) -> i32;
    }

    /// One reference to an `ANativeWindow`, released when dropped.
    pub(super) struct Window(NonNull<ANativeWindow>);

    // SAFETY: an `ANativeWindow` is reference counted and its functions may be called from any
    // thread.
    unsafe impl Send for Window {}
    // SAFETY: as above; `&Window` only hands out the pointer.
    unsafe impl Sync for Window {}

    impl Window {
        /// Takes a reference of its own to `window`; the caller keeps and releases its own.
        ///
        /// # Safety
        ///
        /// `window` is a live `ANativeWindow`.
        pub(super) unsafe fn acquire(window: NonNull<ANativeWindow>) -> Self {
            // SAFETY: live (caller); balanced by the release in `drop`.
            unsafe { ANativeWindow_acquire(window.as_ptr()) };
            Self(window)
        }

        /// Adopts a reference the caller owns (as `AMediaCodec_createInputSurface` gives it).
        fn adopt(window: NonNull<ANativeWindow>) -> Self {
            Self(window)
        }

        pub(super) fn as_ptr(&self) -> *mut ANativeWindow {
            self.0.as_ptr()
        }
    }

    impl Clone for Window {
        fn clone(&self) -> Self {
            // SAFETY: `self` holds a reference, so the window is live.
            unsafe { Self::acquire(self.0) }
        }
    }

    impl Drop for Window {
        fn drop(&mut self) {
            // SAFETY: releases the one reference this value holds.
            unsafe { ANativeWindow_release(self.0.as_ptr()) };
        }
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

    /// Opens a system library for good: the handle is never closed, so every symbol taken from
    /// it stays valid for the life of the process.
    fn open_library(name: &CStr) -> Result<*mut c_void, VideoError> {
        // SAFETY: a NUL-terminated name.
        let library = unsafe { libc::dlopen(name.as_ptr(), libc::RTLD_NOW) };
        if library.is_null() {
            return Err(VideoError::Unsupported);
        }
        Ok(library)
    }

    // ---------------------------------------------------------------------------------------
    // MediaCodec (`libmediandk.so`)
    // ---------------------------------------------------------------------------------------

    #[repr(C)]
    struct AMediaCodec {
        _private: [u8; 0],
    }

    #[repr(C)]
    struct AMediaFormat {
        _private: [u8; 0],
    }

    #[repr(C)]
    struct AImageReader {
        _private: [u8; 0],
    }

    /// `AMediaCodecBufferInfo`.
    #[repr(C)]
    #[derive(Debug, Default, Clone, Copy)]
    pub(super) struct BufferInfo {
        offset: i32,
        size: i32,
        presentation_time_us: i64,
        flags: u32,
    }

    const AMEDIA_OK: i32 = 0;
    const CONFIGURE_FLAG_ENCODE: u32 = 1;
    const INFO_TRY_AGAIN_LATER: isize = -1;
    const INFO_OUTPUT_FORMAT_CHANGED: isize = -2;
    const INFO_OUTPUT_BUFFERS_CHANGED: isize = -3;
    /// `AIMAGE_FORMAT_PRIVATE`: the format of surfaces fed by the camera or a decoder.
    const IMAGE_FORMAT_PRIVATE: i32 = 0x22;
    /// `AHARDWAREBUFFER_USAGE_GPU_SAMPLED_IMAGE`.
    const USAGE_GPU_SAMPLED_IMAGE: u64 = 1 << 8;
    /// How long the drain threads wait for an output buffer before checking whether to stop.
    const OUTPUT_TIMEOUT_US: i64 = 10_000;
    /// How long `push` waits for a free decoder input buffer before dropping the frame.
    const INPUT_TIMEOUT_US: i64 = 5_000;

    type CodecCall = unsafe extern "C" fn(*mut AMediaCodec) -> i32;

    /// The MediaCodec functions, looked up at run time.
    ///
    /// Why not link `libmediandk.so`: the functions the encoder needs (input surface, runtime
    /// parameters) are Android 8.0 (API 26), and the app's minimum is Android 7 (API 24); a
    /// linked symbol that is missing would stop the whole native library from loading there.
    struct Media {
        create_encoder: unsafe extern "C" fn(*const c_char) -> *mut AMediaCodec,
        create_decoder: unsafe extern "C" fn(*const c_char) -> *mut AMediaCodec,
        configure: unsafe extern "C" fn(
            *mut AMediaCodec,
            *const AMediaFormat,
            *mut ANativeWindow,
            *mut c_void,
            u32,
        ) -> i32,
        start: CodecCall,
        stop: CodecCall,
        delete: CodecCall,
        dequeue_input: unsafe extern "C" fn(*mut AMediaCodec, i64) -> isize,
        input_buffer: unsafe extern "C" fn(*mut AMediaCodec, usize, *mut usize) -> *mut u8,
        queue_input:
            unsafe extern "C" fn(*mut AMediaCodec, usize, libc::off_t, usize, u64, u32) -> i32,
        dequeue_output: unsafe extern "C" fn(*mut AMediaCodec, *mut BufferInfo, i64) -> isize,
        output_buffer: unsafe extern "C" fn(*mut AMediaCodec, usize, *mut usize) -> *mut u8,
        release_output: unsafe extern "C" fn(*mut AMediaCodec, usize, bool) -> i32,
        set_output_surface: unsafe extern "C" fn(*mut AMediaCodec, *mut ANativeWindow) -> i32,
        format_new: unsafe extern "C" fn() -> *mut AMediaFormat,
        format_delete: unsafe extern "C" fn(*mut AMediaFormat) -> i32,
        set_int32: unsafe extern "C" fn(*mut AMediaFormat, *const c_char, i32),
        set_string: unsafe extern "C" fn(*mut AMediaFormat, *const c_char, *const c_char),
        set_buffer: unsafe extern "C" fn(*mut AMediaFormat, *const c_char, *const c_void, usize),
        /// Android 8.0 (API 26) and later: what the camera source needs.
        encoder: Option<EncoderApi>,
        /// Android 8.0 (API 26) and later: a surface for the decoder while the app has none.
        new_image_reader:
            Option<unsafe extern "C" fn(i32, i32, i32, u64, i32, *mut *mut AImageReader) -> i32>,
        image_reader_window:
            unsafe extern "C" fn(*mut AImageReader, *mut *mut ANativeWindow) -> i32,
        image_reader_delete: unsafe extern "C" fn(*mut AImageReader),
    }

    struct EncoderApi {
        create_input_surface:
            unsafe extern "C" fn(*mut AMediaCodec, *mut *mut ANativeWindow) -> i32,
        set_parameters: unsafe extern "C" fn(*mut AMediaCodec, *const AMediaFormat) -> i32,
    }

    impl Media {
        /// MediaCodec, loaded once per process.
        fn get() -> Result<&'static Media, VideoError> {
            static MEDIA: OnceLock<Result<Media, VideoError>> = OnceLock::new();
            MEDIA
                .get_or_init(Media::load)
                .as_ref()
                .map_err(Clone::clone)
        }

        fn load() -> Result<Media, VideoError> {
            let library = open_library(c"libmediandk.so")?;
            macro_rules! required {
                ($name:literal) => {
                    // SAFETY: the field this lands in has the type of the C declaration.
                    unsafe { symbol(library, $name) }.ok_or_else(|| {
                        VideoError::Backend(format!("MediaCodec has no {:?}", $name))
                    })?
                };
            }
            macro_rules! optional {
                ($name:literal) => {
                    // SAFETY: as above.
                    unsafe { symbol(library, $name) }
                };
            }
            let encoder = (|| {
                Some(EncoderApi {
                    create_input_surface: optional!(c"AMediaCodec_createInputSurface")?,
                    set_parameters: optional!(c"AMediaCodec_setParameters")?,
                })
            })();
            Ok(Media {
                create_encoder: required!(c"AMediaCodec_createEncoderByType"),
                create_decoder: required!(c"AMediaCodec_createDecoderByType"),
                configure: required!(c"AMediaCodec_configure"),
                start: required!(c"AMediaCodec_start"),
                stop: required!(c"AMediaCodec_stop"),
                delete: required!(c"AMediaCodec_delete"),
                dequeue_input: required!(c"AMediaCodec_dequeueInputBuffer"),
                input_buffer: required!(c"AMediaCodec_getInputBuffer"),
                queue_input: required!(c"AMediaCodec_queueInputBuffer"),
                dequeue_output: required!(c"AMediaCodec_dequeueOutputBuffer"),
                output_buffer: required!(c"AMediaCodec_getOutputBuffer"),
                release_output: required!(c"AMediaCodec_releaseOutputBuffer"),
                set_output_surface: required!(c"AMediaCodec_setOutputSurface"),
                format_new: required!(c"AMediaFormat_new"),
                format_delete: required!(c"AMediaFormat_delete"),
                set_int32: required!(c"AMediaFormat_setInt32"),
                set_string: required!(c"AMediaFormat_setString"),
                set_buffer: required!(c"AMediaFormat_setBuffer"),
                encoder,
                new_image_reader: optional!(c"AImageReader_newWithUsage"),
                image_reader_window: required!(c"AImageReader_getWindow"),
                image_reader_delete: required!(c"AImageReader_delete"),
            })
        }
    }

    fn media_error(what: &str, status: impl std::fmt::Display) -> VideoError {
        VideoError::Backend(format!("MediaCodec {what}: {status}"))
    }

    /// An `AMediaFormat`, deleted when dropped.
    struct Format {
        media: &'static Media,
        raw: NonNull<AMediaFormat>,
    }

    impl Format {
        fn new(media: &'static Media, entries: &[FormatEntry]) -> Result<Self, VideoError> {
            // SAFETY: no arguments; a null result is handled.
            let raw = NonNull::new(unsafe { (media.format_new)() })
                .ok_or_else(|| media_error("format", "null"))?;
            let format = Self { media, raw };
            for (key, value) in entries {
                let raw = format.raw.as_ptr();
                // SAFETY: the format is live; keys and strings are NUL-terminated and static;
                // `setBuffer` copies the bytes.
                unsafe {
                    match value {
                        FormatValue::Int(value) => (media.set_int32)(raw, key.as_ptr(), *value),
                        FormatValue::Str(value) => {
                            (media.set_string)(raw, key.as_ptr(), value.as_ptr())
                        }
                        FormatValue::Buffer(bytes) => (media.set_buffer)(
                            raw,
                            key.as_ptr(),
                            bytes.as_ptr().cast(),
                            bytes.len(),
                        ),
                    }
                }
            }
            Ok(format)
        }
    }

    impl Drop for Format {
        fn drop(&mut self) {
            // SAFETY: created in `new`, deleted once.
            unsafe { (self.media.format_delete)(self.raw.as_ptr()) };
        }
    }

    /// An `AMediaCodec`, stopped and deleted when dropped.
    pub(super) struct Codec {
        media: &'static Media,
        raw: NonNull<AMediaCodec>,
    }

    // SAFETY: MediaCodec serialises its calls internally (each one is a message to the codec's
    // looper), so a codec may be driven from several threads: the owner configures, sets
    // parameters and queues input, a drain thread dequeues and releases output.
    unsafe impl Send for Codec {}
    // SAFETY: as above.
    unsafe impl Sync for Codec {}

    impl Codec {
        fn create(media: &'static Media, encoder: bool) -> Result<Self, VideoError> {
            let create = if encoder {
                media.create_encoder
            } else {
                media.create_decoder
            };
            // SAFETY: a NUL-terminated MIME type; a null result is handled.
            let raw = NonNull::new(unsafe { create(MIME_AVC.as_ptr()) })
                .ok_or(VideoError::Unsupported)?;
            Ok(Self { media, raw })
        }

        fn configure(
            &self,
            format: &Format,
            window: *mut ANativeWindow,
            flags: u32,
        ) -> Result<(), VideoError> {
            // SAFETY: codec and format are live; `window` is null or live (callers hold a
            // reference for as long as the codec renders to it); no crypto.
            let status = unsafe {
                (self.media.configure)(
                    self.raw.as_ptr(),
                    format.raw.as_ptr(),
                    window,
                    ptr::null_mut(),
                    flags,
                )
            };
            match status {
                AMEDIA_OK => Ok(()),
                status => Err(media_error("configure", status)),
            }
        }

        fn start(&self) -> Result<(), VideoError> {
            // SAFETY: live and configured.
            match unsafe { (self.media.start)(self.raw.as_ptr()) } {
                AMEDIA_OK => Ok(()),
                status => Err(media_error("start", status)),
            }
        }

        /// Changes parameters of the running codec (`AMediaCodec_setParameters`, API 26).
        fn set_parameters(&self, entries: &[FormatEntry]) -> Result<(), VideoError> {
            let api = self.media.encoder.as_ref().ok_or(VideoError::Unsupported)?;
            let format = Format::new(self.media, entries)?;
            // SAFETY: codec and format are live; the codec copies what it needs.
            match unsafe { (api.set_parameters)(self.raw.as_ptr(), format.raw.as_ptr()) } {
                AMEDIA_OK => Ok(()),
                status => Err(media_error("set parameters", status)),
            }
        }

        /// The encoder's input surface (`AMediaCodec_createInputSurface`, API 26), between
        /// `configure` and `start`.
        fn create_input_surface(&self) -> Result<Window, VideoError> {
            let api = self.media.encoder.as_ref().ok_or(VideoError::Unsupported)?;
            let mut window = ptr::null_mut();
            // SAFETY: the codec is live and configured as an encoder; `window` is a valid place.
            let status = unsafe { (api.create_input_surface)(self.raw.as_ptr(), &mut window) };
            match NonNull::new(window) {
                Some(window) if status == AMEDIA_OK => Ok(Window::adopt(window)),
                _ => Err(media_error("input surface", status)),
            }
        }

        /// The next output buffer: its index, or an `INFO_*` code or error (negative).
        fn dequeue_output(&self, info: &mut BufferInfo, timeout_us: i64) -> isize {
            // SAFETY: the codec is live and `info` a valid place.
            unsafe { (self.media.dequeue_output)(self.raw.as_ptr(), info, timeout_us) }
        }

        /// Runs `read` on the bytes of the dequeued output buffer `index`, before it is
        /// released.
        fn read_output<T>(
            &self,
            index: usize,
            info: &BufferInfo,
            read: impl FnOnce(&[u8]) -> T,
        ) -> Option<T> {
            let mut capacity = 0;
            // SAFETY: `index` was dequeued and not released yet; `capacity` a valid place.
            let base =
                unsafe { (self.media.output_buffer)(self.raw.as_ptr(), index, &mut capacity) };
            let offset = usize::try_from(info.offset).ok()?;
            let size = usize::try_from(info.size).ok()?;
            if base.is_null() || size == 0 || offset.checked_add(size)? > capacity {
                return None;
            }
            // SAFETY: `offset + size` is within the buffer's `capacity` bytes, which stay valid
            // until the buffer is released, after `read` returns.
            let bytes = unsafe { std::slice::from_raw_parts(base.add(offset), size) };
            Some(read(bytes))
        }

        fn release_output(&self, index: usize, render: bool) {
            // SAFETY: `index` was dequeued and is released once. A failure (the surface went
            // away) shows up in the next dequeue.
            unsafe { (self.media.release_output)(self.raw.as_ptr(), index, render) };
        }

        /// Copies one access unit into an input buffer and queues it. `Ok(false)` if no input
        /// buffer was free in time, or the unit did not fit.
        fn queue_input(&self, data: &[u8], time_us: u64) -> Result<bool, VideoError> {
            // SAFETY: the codec is live.
            let index = unsafe { (self.media.dequeue_input)(self.raw.as_ptr(), INPUT_TIMEOUT_US) };
            if index == INFO_TRY_AGAIN_LATER {
                return Ok(false);
            }
            let index = usize::try_from(index).map_err(|_| media_error("input", index))?;
            let mut capacity = 0;
            // SAFETY: `index` was just dequeued; `capacity` is a valid place.
            let buffer =
                unsafe { (self.media.input_buffer)(self.raw.as_ptr(), index, &mut capacity) };
            let fits = !buffer.is_null() && data.len() <= capacity;
            let size = if fits {
                // SAFETY: `buffer` has `capacity` writable bytes until queued, and `data` fits;
                // the two do not overlap.
                unsafe { ptr::copy_nonoverlapping(data.as_ptr(), buffer, data.len()) };
                data.len()
            } else {
                // The buffer goes back empty.
                0
            };
            // SAFETY: `index` was dequeued and is queued once, with `size` bytes written.
            let status =
                unsafe { (self.media.queue_input)(self.raw.as_ptr(), index, 0, size, time_us, 0) };
            match status {
                AMEDIA_OK => Ok(fits),
                status => Err(media_error("queue input", status)),
            }
        }

        fn set_output_surface(&self, window: *mut ANativeWindow) -> Result<(), VideoError> {
            // SAFETY: the codec is live; `window` is live while the codec renders to it
            // (callers hold a reference).
            match unsafe { (self.media.set_output_surface)(self.raw.as_ptr(), window) } {
                AMEDIA_OK => Ok(()),
                status => Err(media_error("set output surface", status)),
            }
        }
    }

    impl Drop for Codec {
        fn drop(&mut self) {
            // SAFETY: live until here and deleted once. `stop` on a codec that never started
            // fails harmlessly; `delete` frees it in any state.
            unsafe {
                (self.media.stop)(self.raw.as_ptr());
                (self.media.delete)(self.raw.as_ptr());
            }
        }
    }

    // ---------------------------------------------------------------------------------------
    // Encoder
    // ---------------------------------------------------------------------------------------

    /// What the encoder's drain thread reads from its owner.
    #[derive(Default)]
    pub(super) struct EncoderShared {
        /// The [`Rotation`] of the frames, as a CVO value.
        rotation: AtomicU8,
        /// The engine asked for a keyframe (the drain thread notes it in its bookkeeping).
        keyframe_requested: AtomicBool,
        /// The encoder failed: it stopped producing frames.
        failed: AtomicBool,
    }

    impl EncoderShared {
        pub(super) fn set_rotation(&self, rotation: Rotation) {
            self.rotation.store(rotation.to_cvo(), Ordering::Relaxed);
        }

        fn rotation(&self) -> Rotation {
            Rotation::from_cvo(self.rotation.load(Ordering::Relaxed))
        }
    }

    /// The hardware H.264 encoder fed by its input surface, with the thread that drains it
    /// into the [`FrameSender`].
    pub(super) struct Encoder {
        codec: Arc<Codec>,
        /// Dropped after the codec: the camera writes here.
        input: Window,
        size: (u32, u32),
        shared: Arc<EncoderShared>,
        stop: Arc<AtomicBool>,
        thread: Option<JoinHandle<()>>,
    }

    impl Encoder {
        /// Configures and starts an encoder for `size` frames and starts draining it.
        pub(super) fn start(
            size: (u32, u32),
            config: &VideoConfig,
            out: FrameSender,
            shared: Arc<EncoderShared>,
        ) -> Result<Self, VideoError> {
            let media = Media::get()?;
            if media.encoder.is_none() {
                // Android 7: no input surface and no runtime bitrate. See the module docs.
                return Err(VideoError::Unsupported);
            }
            let mut last_error = VideoError::Unsupported;
            let mut configured = None;
            for entries in encoder_formats(size.0, size.1, config) {
                let codec = Codec::create(media, true)?;
                let format = Format::new(media, &entries)?;
                match codec.configure(&format, ptr::null_mut(), CONFIGURE_FLAG_ENCODE) {
                    Ok(()) => {
                        configured = Some(codec);
                        break;
                    }
                    Err(error) => last_error = error,
                }
            }
            let codec = Arc::new(configured.ok_or(last_error)?);
            let input = codec.create_input_surface()?;
            codec.start()?;

            let stop = Arc::new(AtomicBool::new(false));
            let thread = thread::Builder::new()
                .name("ft-video-encode".to_owned())
                .spawn({
                    let codec = codec.clone();
                    let shared = shared.clone();
                    let stop = stop.clone();
                    move || drain_encoder(&codec, &out, &shared, &stop)
                })
                .map_err(|error| VideoError::Backend(format!("encoder thread: {error}")))?;
            Ok(Self {
                codec,
                input,
                size,
                shared,
                stop,
                thread: Some(thread),
            })
        }

        /// The surface the camera writes the frames to.
        pub(super) fn input(&self) -> &Window {
            &self.input
        }

        pub(super) fn size(&self) -> (u32, u32) {
            self.size
        }

        pub(super) fn request_keyframe(&self) {
            self.shared
                .keyframe_requested
                .store(true, Ordering::Relaxed);
            // A failure leaves the regular keyframe every two seconds.
            let _ = self.codec.set_parameters(&keyframe_parameters());
        }

        pub(super) fn set_bitrate(&self, bps: u32) {
            // A failure leaves the old bitrate.
            let _ = self.codec.set_parameters(&bitrate_parameters(bps));
        }
    }

    impl Drop for Encoder {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    /// The encoder's drain loop: output buffers → [`AccessUnitPackager`] → [`FrameSender`],
    /// asking for keyframes when the channel needs one. Runs until `stop` or a codec error.
    fn drain_encoder(codec: &Codec, out: &FrameSender, shared: &EncoderShared, stop: &AtomicBool) {
        let mut packager = AccessUnitPackager::default();
        let mut requests = KeyframeRequests::default();
        let mut info = BufferInfo::default();
        while !stop.load(Ordering::Relaxed) {
            let index = codec.dequeue_output(&mut info, OUTPUT_TIMEOUT_US);
            let Ok(index) = usize::try_from(index) else {
                if matches!(
                    index,
                    INFO_TRY_AGAIN_LATER | INFO_OUTPUT_FORMAT_CHANGED | INFO_OUTPUT_BUFFERS_CHANGED
                ) {
                    continue;
                }
                shared.failed.store(true, Ordering::Relaxed);
                return;
            };
            let packaged = codec
                .read_output(index, &info, |data| packager.package(data, info.flags))
                .flatten();
            codec.release_output(index, false);
            let Some((data, keyframe)) = packaged else {
                continue;
            };
            let frame = EncodedFrame {
                data,
                keyframe,
                timestamp: Duration::from_micros(
                    u64::try_from(info.presentation_time_us).unwrap_or(0),
                ),
                rotation: shared.rotation(),
            };
            // `Closed`: the engine is gone; keep draining so the encoder does not stall
            // until the owner stops it.
            let _ = out.try_send(frame);
            if shared.keyframe_requested.swap(false, Ordering::Relaxed) {
                requests.requested();
            }
            if requests.after_frame(keyframe, out.keyframe_needed()) {
                let _ = codec.set_parameters(&keyframe_parameters());
            }
        }
    }

    // ---------------------------------------------------------------------------------------
    // Camera2 (`libcamera2ndk.so`)
    // ---------------------------------------------------------------------------------------

    #[repr(C)]
    struct ACameraManager {
        _private: [u8; 0],
    }
    #[repr(C)]
    struct ACameraDevice {
        _private: [u8; 0],
    }
    #[repr(C)]
    struct ACameraMetadata {
        _private: [u8; 0],
    }
    #[repr(C)]
    struct ACaptureRequest {
        _private: [u8; 0],
    }
    #[repr(C)]
    struct ACameraOutputTarget {
        _private: [u8; 0],
    }
    #[repr(C)]
    struct ACaptureSessionOutput {
        _private: [u8; 0],
    }
    #[repr(C)]
    struct ACaptureSessionOutputContainer {
        _private: [u8; 0],
    }
    #[repr(C)]
    struct ACameraCaptureSession {
        _private: [u8; 0],
    }

    /// `ACameraIdList`.
    #[repr(C)]
    struct ACameraIdList {
        count: c_int,
        ids: *const *const c_char,
    }

    /// `ACameraMetadata_const_entry`; `data` is the union of typed pointers.
    #[repr(C)]
    struct ConstEntry {
        tag: u32,
        kind: u8,
        count: u32,
        data: *const c_void,
    }

    /// `ACameraDevice_StateCallbacks`.
    #[repr(C)]
    struct DeviceCallbacks {
        context: *mut c_void,
        on_disconnected: unsafe extern "C" fn(*mut c_void, *mut ACameraDevice),
        on_error: unsafe extern "C" fn(*mut c_void, *mut ACameraDevice, c_int),
    }

    /// `ACameraCaptureSession_stateCallbacks`.
    #[repr(C)]
    struct SessionCallbacks {
        context: *mut c_void,
        on_closed: SessionCallback,
        on_ready: SessionCallback,
        on_active: SessionCallback,
    }

    type SessionCallback = unsafe extern "C" fn(*mut c_void, *mut ACameraCaptureSession);

    const CAMERA_OK: i32 = 0;
    /// `TEMPLATE_RECORD`: steady frame rate, video-grade auto exposure and focus.
    const TEMPLATE_RECORD: c_int = 3;
    const TAG_LENS_FACING: u32 = 0x8_0005;
    const TAG_SENSOR_ORIENTATION: u32 = 0xE_000E;
    const TAG_STREAM_CONFIGURATIONS: u32 = 0xD_000A;
    const TAG_AE_AVAILABLE_FPS_RANGES: u32 = 0x1_0014;
    const TAG_AE_TARGET_FPS_RANGE: u32 = 0x1_0005;

    type CameraCall<T> = unsafe extern "C" fn(*mut T) -> i32;

    /// The Camera2 NDK functions, looked up at run time (Android 7.0, API 24, has them all;
    /// looked up anyway so a device without the library fails in `start`, not at load).
    struct Camera2 {
        manager_create: unsafe extern "C" fn() -> *mut ACameraManager,
        manager_delete: unsafe extern "C" fn(*mut ACameraManager),
        id_list: unsafe extern "C" fn(*mut ACameraManager, *mut *mut ACameraIdList) -> i32,
        delete_id_list: unsafe extern "C" fn(*mut ACameraIdList),
        characteristics: unsafe extern "C" fn(
            *mut ACameraManager,
            *const c_char,
            *mut *mut ACameraMetadata,
        ) -> i32,
        metadata_free: unsafe extern "C" fn(*mut ACameraMetadata),
        const_entry: unsafe extern "C" fn(*const ACameraMetadata, u32, *mut ConstEntry) -> i32,
        open_camera: unsafe extern "C" fn(
            *mut ACameraManager,
            *const c_char,
            *mut DeviceCallbacks,
            *mut *mut ACameraDevice,
        ) -> i32,
        device_close: CameraCall<ACameraDevice>,
        create_request:
            unsafe extern "C" fn(*const ACameraDevice, c_int, *mut *mut ACaptureRequest) -> i32,
        request_free: unsafe extern "C" fn(*mut ACaptureRequest),
        request_add_target:
            unsafe extern "C" fn(*mut ACaptureRequest, *const ACameraOutputTarget) -> i32,
        request_set_i32: unsafe extern "C" fn(*mut ACaptureRequest, u32, u32, *const i32) -> i32,
        target_create:
            unsafe extern "C" fn(*mut ANativeWindow, *mut *mut ACameraOutputTarget) -> i32,
        target_free: unsafe extern "C" fn(*mut ACameraOutputTarget),
        container_create: unsafe extern "C" fn(*mut *mut ACaptureSessionOutputContainer) -> i32,
        container_free: unsafe extern "C" fn(*mut ACaptureSessionOutputContainer),
        container_add: unsafe extern "C" fn(
            *mut ACaptureSessionOutputContainer,
            *const ACaptureSessionOutput,
        ) -> i32,
        output_create:
            unsafe extern "C" fn(*mut ANativeWindow, *mut *mut ACaptureSessionOutput) -> i32,
        output_free: unsafe extern "C" fn(*mut ACaptureSessionOutput),
        create_session: unsafe extern "C" fn(
            *mut ACameraDevice,
            *const ACaptureSessionOutputContainer,
            *const SessionCallbacks,
            *mut *mut ACameraCaptureSession,
        ) -> i32,
        set_repeating: unsafe extern "C" fn(
            *mut ACameraCaptureSession,
            *mut c_void,
            c_int,
            *mut *mut ACaptureRequest,
            *mut c_int,
        ) -> i32,
        session_close: unsafe extern "C" fn(*mut ACameraCaptureSession),
        stop_repeating: CameraCall<ACameraCaptureSession>,
    }

    impl Camera2 {
        fn get() -> Result<&'static Camera2, VideoError> {
            static CAMERA: OnceLock<Result<Camera2, VideoError>> = OnceLock::new();
            CAMERA
                .get_or_init(Camera2::load)
                .as_ref()
                .map_err(Clone::clone)
        }

        fn load() -> Result<Camera2, VideoError> {
            let library = open_library(c"libcamera2ndk.so")?;
            macro_rules! required {
                ($name:literal) => {
                    // SAFETY: the field this lands in has the type of the C declaration.
                    unsafe { symbol(library, $name) }
                        .ok_or_else(|| VideoError::Backend(format!("Camera2 has no {:?}", $name)))?
                };
            }
            Ok(Camera2 {
                manager_create: required!(c"ACameraManager_create"),
                manager_delete: required!(c"ACameraManager_delete"),
                id_list: required!(c"ACameraManager_getCameraIdList"),
                delete_id_list: required!(c"ACameraManager_deleteCameraIdList"),
                characteristics: required!(c"ACameraManager_getCameraCharacteristics"),
                metadata_free: required!(c"ACameraMetadata_free"),
                const_entry: required!(c"ACameraMetadata_getConstEntry"),
                open_camera: required!(c"ACameraManager_openCamera"),
                device_close: required!(c"ACameraDevice_close"),
                create_request: required!(c"ACameraDevice_createCaptureRequest"),
                request_free: required!(c"ACaptureRequest_free"),
                request_add_target: required!(c"ACaptureRequest_addTarget"),
                request_set_i32: required!(c"ACaptureRequest_setEntry_i32"),
                target_create: required!(c"ACameraOutputTarget_create"),
                target_free: required!(c"ACameraOutputTarget_free"),
                container_create: required!(c"ACaptureSessionOutputContainer_create"),
                container_free: required!(c"ACaptureSessionOutputContainer_free"),
                container_add: required!(c"ACaptureSessionOutputContainer_add"),
                output_create: required!(c"ACaptureSessionOutput_create"),
                output_free: required!(c"ACaptureSessionOutput_free"),
                create_session: required!(c"ACameraDevice_createCaptureSession"),
                set_repeating: required!(c"ACameraCaptureSession_setRepeatingRequest"),
                session_close: required!(c"ACameraCaptureSession_close"),
                stop_repeating: required!(c"ACameraCaptureSession_stopRepeating"),
            })
        }
    }

    /// Turns a `camera_status_t` into a result.
    fn camera_check(status: i32) -> Result<(), VideoError> {
        match status {
            CAMERA_OK => Ok(()),
            status => Err(camera_error(status)),
        }
    }

    /// What the backend needs to know about one camera.
    #[derive(Debug, Clone)]
    pub(super) struct CameraInfo {
        id: CString,
        pub(super) facing: Facing,
        pub(super) orientation: i32,
        pub(super) sizes: Vec<(u32, u32)>,
        fps_ranges: Vec<i32>,
    }

    /// An `ACameraManager`, deleted when dropped.
    pub(super) struct Manager {
        api: &'static Camera2,
        raw: NonNull<ACameraManager>,
    }

    // SAFETY: the NDK camera objects may be used from any thread; the manager is only used by
    // its owner.
    unsafe impl Send for Manager {}

    impl Manager {
        pub(super) fn new() -> Result<Self, VideoError> {
            let api = Camera2::get()?;
            // SAFETY: no arguments; a null result is handled.
            let raw = NonNull::new(unsafe { (api.manager_create)() })
                .ok_or_else(|| VideoError::Backend("no camera manager".to_owned()))?;
            Ok(Self { api, raw })
        }

        /// The front and back cameras, in the order the system lists them.
        pub(super) fn cameras(&self) -> Result<Vec<CameraInfo>, VideoError> {
            let mut list = ptr::null_mut();
            // SAFETY: the manager is live and `list` a valid place.
            camera_check(unsafe { (self.api.id_list)(self.raw.as_ptr(), &mut list) })?;
            let Some(list) = NonNull::new(list) else {
                return Err(VideoError::NoCamera);
            };
            // SAFETY: a list from `getCameraIdList`: `count` NUL-terminated ids, valid until
            // it is deleted below.
            let ids: Vec<CString> = unsafe {
                let list = list.as_ref();
                let count = usize::try_from(list.count).unwrap_or(0);
                if list.ids.is_null() {
                    Vec::new()
                } else {
                    std::slice::from_raw_parts(list.ids, count)
                        .iter()
                        .filter(|id| !id.is_null())
                        .map(|&id| CStr::from_ptr(id).to_owned())
                        .collect()
                }
            };
            // SAFETY: from `getCameraIdList`, deleted once; the ids were copied.
            unsafe { (self.api.delete_id_list)(list.as_ptr()) };
            Ok(ids
                .into_iter()
                .filter_map(|id| self.camera_info(id))
                .collect())
        }

        fn camera_info(&self, id: CString) -> Option<CameraInfo> {
            let mut metadata = ptr::null_mut();
            // SAFETY: the manager is live, `id` NUL-terminated, `metadata` a valid place.
            let status = unsafe {
                (self.api.characteristics)(self.raw.as_ptr(), id.as_ptr(), &mut metadata)
            };
            let metadata = NonNull::new(metadata).filter(|_| status == CAMERA_OK)?;
            let entry = |tag: u32| {
                let mut entry = ConstEntry {
                    tag: 0,
                    kind: 0,
                    count: 0,
                    data: ptr::null(),
                };
                // SAFETY: the metadata is live until freed below; `entry` a valid place.
                let status = unsafe { (self.api.const_entry)(metadata.as_ptr(), tag, &mut entry) };
                (status == CAMERA_OK && !entry.data.is_null()).then_some(entry)
            };
            let i32s = |tag: u32| -> Vec<i32> {
                entry(tag)
                    .map(|entry| {
                        // SAFETY: an int32 entry of `count` values, valid until the metadata
                        // is freed; copied out here.
                        unsafe {
                            std::slice::from_raw_parts(
                                entry.data.cast::<i32>(),
                                usize::try_from(entry.count).unwrap_or(0),
                            )
                        }
                        .to_vec()
                    })
                    .unwrap_or_default()
            };
            let facing = entry(TAG_LENS_FACING)
                .filter(|entry| entry.count >= 1)
                // SAFETY: a byte entry with at least one value.
                .map(|entry| unsafe { *entry.data.cast::<u8>() })
                .and_then(lens_facing);
            let orientation = i32s(TAG_SENSOR_ORIENTATION).first().copied().unwrap_or(0);
            let sizes = output_sizes(&i32s(TAG_STREAM_CONFIGURATIONS), IMAGE_FORMAT_PRIVATE);
            let fps_ranges = i32s(TAG_AE_AVAILABLE_FPS_RANGES);
            // SAFETY: from `getCameraCharacteristics`, freed once; everything was copied.
            unsafe { (self.api.metadata_free)(metadata.as_ptr()) };
            Some(CameraInfo {
                id,
                facing: facing?,
                orientation,
                sizes,
                fps_ranges,
            })
        }

        /// Opens `camera`. Camera errors report to `health`, which must outlive the device.
        fn open(
            &self,
            camera: &CameraInfo,
            health: &Arc<CameraHealth>,
        ) -> Result<CameraDevice, VideoError> {
            let mut callbacks = Box::new(DeviceCallbacks {
                context: Arc::as_ptr(health).cast_mut().cast(),
                on_disconnected: on_camera_disconnected,
                on_error: on_camera_error,
            });
            let mut raw = ptr::null_mut();
            // SAFETY: the manager is live, the id NUL-terminated, the callbacks live as long
            // as the device (they move into it), and `raw` a valid place.
            let status = unsafe {
                (self.api.open_camera)(
                    self.raw.as_ptr(),
                    camera.id.as_ptr(),
                    &mut *callbacks,
                    &mut raw,
                )
            };
            camera_check(status)?;
            let raw = NonNull::new(raw).ok_or_else(|| camera_error(status))?;
            Ok(CameraDevice {
                api: self.api,
                raw,
                _callbacks: callbacks,
            })
        }
    }

    impl Drop for Manager {
        fn drop(&mut self) {
            // SAFETY: created in `new`, deleted once, after every device from it is closed.
            unsafe { (self.api.manager_delete)(self.raw.as_ptr()) };
        }
    }

    /// What the camera's callbacks report. Atomics only: they run on a camera thread.
    #[derive(Default)]
    pub(super) struct CameraHealth {
        lost: AtomicBool,
        error: AtomicI32,
    }

    /// `ACameraDevice_StateCallback`: the camera went to another app or was unplugged.
    ///
    /// # Safety
    ///
    /// `context` is null or a live [`CameraHealth`].
    unsafe extern "C" fn on_camera_disconnected(context: *mut c_void, _device: *mut ACameraDevice) {
        // SAFETY: null or a live `CameraHealth` (caller), read through a shared reference.
        if let Some(health) = unsafe { context.cast::<CameraHealth>().as_ref() } {
            health.lost.store(true, Ordering::Release);
        }
    }

    /// `ACameraDevice_ErrorStateCallback`: the camera failed.
    ///
    /// # Safety
    ///
    /// As [`on_camera_disconnected`].
    unsafe extern "C" fn on_camera_error(
        context: *mut c_void,
        _device: *mut ACameraDevice,
        error: c_int,
    ) {
        // SAFETY: as above.
        if let Some(health) = unsafe { context.cast::<CameraHealth>().as_ref() } {
            health.error.store(error, Ordering::Relaxed);
            health.lost.store(true, Ordering::Release);
        }
    }

    /// How long closing a session waits for the camera to confirm. Closing the device before
    /// the session is closed left the camera unable to drain on a Lenovo tablet (MediaTek),
    /// holding on to the surfaces so the next camera could not use them.
    const SESSION_CLOSE_TIMEOUT: Duration = Duration::from_secs(1);

    /// A session's `onClosed`, for its owner to wait on.
    #[derive(Default)]
    struct SessionSignal {
        closed: Mutex<bool>,
        changed: Condvar,
    }

    impl SessionSignal {
        fn notify_closed(&self) {
            if let Ok(mut closed) = self.closed.lock() {
                *closed = true;
                self.changed.notify_all();
            }
        }

        /// Whether the session closed within `timeout`.
        fn wait_closed(&self, timeout: Duration) -> bool {
            let Ok(closed) = self.closed.lock() else {
                return false;
            };
            self.changed
                .wait_timeout_while(closed, timeout, |closed| !*closed)
                .map(|(closed, _)| *closed)
                .unwrap_or(false)
        }
    }

    /// `ACameraCaptureSession_stateCallback` for `onClosed`. Runs on a camera thread, not a
    /// real-time one: a short lock is fine.
    ///
    /// # Safety
    ///
    /// `context` is null or a live [`SessionSignal`].
    unsafe extern "C" fn on_session_closed(
        context: *mut c_void,
        _session: *mut ACameraCaptureSession,
    ) {
        // SAFETY: null or a live `SessionSignal` (caller), read through a shared reference.
        if let Some(signal) = unsafe { context.cast::<SessionSignal>().as_ref() } {
            signal.notify_closed();
        }
    }

    /// `onReady` and `onActive`: nothing to do.
    unsafe extern "C" fn on_session_state(
        _context: *mut c_void,
        _session: *mut ACameraCaptureSession,
    ) {
    }

    /// An open `ACameraDevice`, closed when dropped.
    pub(super) struct CameraDevice {
        api: &'static Camera2,
        raw: NonNull<ACameraDevice>,
        _callbacks: Box<DeviceCallbacks>,
    }

    // SAFETY: the NDK camera objects may be used from any thread.
    unsafe impl Send for CameraDevice {}

    impl Drop for CameraDevice {
        fn drop(&mut self) {
            // SAFETY: open until here, closed once; no callback comes after it returns.
            unsafe { (self.api.device_close)(self.raw.as_ptr()) };
        }
    }

    /// A repeating capture from a camera into its surfaces, closed when dropped.
    pub(super) struct CaptureSession {
        api: &'static Camera2,
        container: Option<NonNull<ACaptureSessionOutputContainer>>,
        outputs: Vec<NonNull<ACaptureSessionOutput>>,
        request: Option<NonNull<ACaptureRequest>>,
        targets: Vec<NonNull<ACameraOutputTarget>>,
        session: Option<NonNull<ACameraCaptureSession>>,
        /// The callbacks; their context is one strong count of `signal`, from
        /// `Arc::into_raw`.
        callbacks: Box<SessionCallbacks>,
        signal: Arc<SessionSignal>,
        /// Held while the camera writes to them.
        windows: Vec<Window>,
    }

    // SAFETY: the NDK camera objects may be used from any thread.
    unsafe impl Send for CaptureSession {}

    impl CaptureSession {
        /// Starts capturing from `device` into every window, repeatedly.
        pub(super) fn start(
            device: &CameraDevice,
            windows: Vec<Window>,
            fps_range: Option<[i32; 2]>,
        ) -> Result<Self, VideoError> {
            let api = device.api;
            let signal = Arc::new(SessionSignal::default());
            let callbacks = Box::new(SessionCallbacks {
                context: Arc::into_raw(signal.clone()).cast_mut().cast(),
                on_closed: on_session_closed,
                on_ready: on_session_state,
                on_active: on_session_state,
            });
            let mut session = Self {
                api,
                container: None,
                outputs: Vec::new(),
                request: None,
                targets: Vec::new(),
                session: None,
                callbacks,
                signal,
                windows,
            };
            let mut raw = ptr::null_mut();
            // SAFETY: `raw` is a valid place.
            camera_check(unsafe { (api.container_create)(&mut raw) })?;
            let container = NonNull::new(raw).ok_or(VideoError::Unsupported)?;
            session.container = Some(container);

            let mut request = ptr::null_mut();
            // SAFETY: the device is open and `request` a valid place.
            camera_check(unsafe {
                (api.create_request)(device.raw.as_ptr(), TEMPLATE_RECORD, &mut request)
            })?;
            let request = NonNull::new(request).ok_or(VideoError::Unsupported)?;
            session.request = Some(request);

            for index in 0..session.windows.len() {
                let window = session.windows[index].as_ptr();
                let mut output = ptr::null_mut();
                // SAFETY: the window is live (held by the session); `output` a valid place.
                camera_check(unsafe { (api.output_create)(window, &mut output) })?;
                let output = NonNull::new(output).ok_or(VideoError::Unsupported)?;
                session.outputs.push(output);
                // SAFETY: container and output are live.
                camera_check(unsafe { (api.container_add)(container.as_ptr(), output.as_ptr()) })?;

                let mut target = ptr::null_mut();
                // SAFETY: as for the output.
                camera_check(unsafe { (api.target_create)(window, &mut target) })?;
                let target = NonNull::new(target).ok_or(VideoError::Unsupported)?;
                session.targets.push(target);
                // SAFETY: request and target are live.
                camera_check(unsafe {
                    (api.request_add_target)(request.as_ptr(), target.as_ptr())
                })?;
            }
            if let Some(range) = fps_range {
                // SAFETY: the request is live and `range` holds the two values given. A
                // refusal leaves the template's range.
                unsafe {
                    (api.request_set_i32)(
                        request.as_ptr(),
                        TAG_AE_TARGET_FPS_RANGE,
                        2,
                        range.as_ptr(),
                    )
                };
            }

            let mut raw_session = ptr::null_mut();
            // SAFETY: device and container are live; the callbacks and their context outlive
            // the session (see `drop`); `raw_session` a valid place.
            camera_check(unsafe {
                (api.create_session)(
                    device.raw.as_ptr(),
                    container.as_ptr(),
                    &*session.callbacks,
                    &mut raw_session,
                )
            })?;
            let raw_session = NonNull::new(raw_session).ok_or(VideoError::Unsupported)?;
            session.session = Some(raw_session);
            let mut requests = [request.as_ptr()];
            // SAFETY: session and request are live; no capture callbacks; one request.
            camera_check(unsafe {
                (api.set_repeating)(
                    raw_session.as_ptr(),
                    ptr::null_mut(),
                    1,
                    requests.as_mut_ptr(),
                    ptr::null_mut(),
                )
            })?;
            Ok(session)
        }
    }

    impl Drop for CaptureSession {
        fn drop(&mut self) {
            // The callbacks may still come until `onClosed`: without it, their context and
            // struct are leaked rather than freed under the camera.
            let mut callbacks_done = true;
            if let Some(session) = self.session.take() {
                // SAFETY: created in `start`, closed once; stopping the repeating request
                // first lets the camera finish its captures before the close.
                unsafe {
                    (self.api.stop_repeating)(session.as_ptr());
                    (self.api.session_close)(session.as_ptr());
                }
                callbacks_done = self.signal.wait_closed(SESSION_CLOSE_TIMEOUT);
            }
            if callbacks_done {
                // SAFETY: the strong count given to the callbacks in `start`, taken back once,
                // now that no callback comes.
                drop(unsafe { Arc::from_raw(self.callbacks.context.cast::<SessionSignal>()) });
            } else {
                let callbacks = std::mem::replace(
                    &mut self.callbacks,
                    Box::new(SessionCallbacks {
                        context: ptr::null_mut(),
                        on_closed: on_session_state,
                        on_ready: on_session_state,
                        on_active: on_session_state,
                    }),
                );
                std::mem::forget(callbacks);
            }
            // SAFETY: each object was created in `start` and is freed once, after the session.
            unsafe {
                if let Some(request) = self.request.take() {
                    (self.api.request_free)(request.as_ptr());
                }
                for target in self.targets.drain(..) {
                    (self.api.target_free)(target.as_ptr());
                }
                for output in self.outputs.drain(..) {
                    (self.api.output_free)(output.as_ptr());
                }
                if let Some(container) = self.container.take() {
                    (self.api.container_free)(container.as_ptr());
                }
            }
        }
    }

    // ---------------------------------------------------------------------------------------
    // CameraSource
    // ---------------------------------------------------------------------------------------

    /// A running capture. Field order is drop order: the camera stops writing to the encoder's
    /// surface before the encoder goes, and the manager goes last.
    struct Running {
        session: Option<CaptureSession>,
        device: Option<CameraDevice>,
        encoder: Encoder,
        manager: Manager,
        cameras: Vec<CameraInfo>,
        camera: CameraInfo,
        config: VideoConfig,
        out: FrameSender,
        shared: Arc<EncoderShared>,
    }

    /// The camera through Camera2, encoded to H.264 by the hardware encoder (Android 8.0, API
    /// 26, and later). See the [module documentation](super) for the app's side.
    pub struct CameraSource {
        preview: Option<Window>,
        display: Rotation,
        health: Arc<CameraHealth>,
        running: Option<Running>,
    }

    impl Default for CameraSource {
        fn default() -> Self {
            Self::new()
        }
    }

    impl CameraSource {
        pub fn new() -> Self {
            Self {
                preview: None,
                display: Rotation::Deg0,
                health: Arc::default(),
                running: None,
            }
        }

        /// Shows the local preview in `window` (from `ANativeWindow_fromSurface` on the
        /// preview `SurfaceView`'s `Surface`), or stops showing it with `None`. The source
        /// takes a reference of its own: the caller may release its reference right after.
        /// While running, it restarts the capture session (a short gap in the video).
        ///
        /// # Safety
        ///
        /// `window` is `None` or a live `ANativeWindow`.
        pub unsafe fn set_preview_surface(
            &mut self,
            window: Option<NonNull<ANativeWindow>>,
        ) -> Result<(), VideoError> {
            // SAFETY: live (caller).
            self.preview = window.map(|window| unsafe { Window::acquire(window) });
            if self.running.is_some() {
                self.restart_session()?;
            }
            Ok(())
        }

        /// How the app's screen is turned (`Display.getRotation()`): part of the rotation the
        /// frames carry. Call it when the activity turns; a portrait-only app never needs to.
        pub fn set_display_rotation(&mut self, display: Rotation) {
            self.display = display;
            if let Some(running) = &self.running {
                running.shared.set_rotation(frame_rotation(
                    running.camera.orientation,
                    running.camera.facing,
                    display,
                ));
            }
        }

        /// Whether the camera was taken by another app or failed, or the encoder failed, since
        /// `start`: no more frames come. The owner restarts the source (or ends the video) when
        /// it sees it; nothing restarts on its own.
        pub fn camera_lost(&self) -> bool {
            self.health.lost.load(Ordering::Acquire)
                || self
                    .running
                    .as_ref()
                    .is_some_and(|running| running.shared.failed.load(Ordering::Relaxed))
        }

        /// The capture (and encode) size, in sensor orientation, while running.
        pub fn capture_size(&self) -> Option<(u32, u32)> {
            self.running.as_ref().map(|running| running.encoder.size())
        }

        fn restart_session(&mut self) -> Result<(), VideoError> {
            let Some(running) = &mut self.running else {
                return Ok(());
            };
            running.session = None;
            let Some(device) = &running.device else {
                return Err(VideoError::Backend("camera not open".to_owned()));
            };
            let windows = session_windows(&running.encoder, self.preview.as_ref());
            let fps = choose_fps_range(&running.camera.fps_ranges, running.config.fps);
            running.session = Some(CaptureSession::start(device, windows, fps)?);
            Ok(())
        }
    }

    /// The camera's outputs: the encoder's surface and, if there is one, the preview, sized
    /// to the capture so the camera accepts it.
    fn session_windows(encoder: &Encoder, preview: Option<&Window>) -> Vec<Window> {
        let mut windows = vec![encoder.input().clone()];
        if let Some(preview) = preview {
            let (width, height) = encoder.size();
            // SAFETY: the window is live (held). Format 0 keeps the window's format. A refusal
            // leaves the view's size, which the camera may reject in `start`.
            unsafe {
                ANativeWindow_setBuffersGeometry(
                    preview.as_ptr(),
                    i32::try_from(width).unwrap_or(0),
                    i32::try_from(height).unwrap_or(0),
                    0,
                )
            };
            windows.push(preview.clone());
        }
        windows
    }

    /// The capture size for `camera`: from the sizes it shares with the other cameras if any,
    /// so a switch keeps the encoder.
    fn capture_size(
        cameras: &[CameraInfo],
        camera: &CameraInfo,
        config: &VideoConfig,
    ) -> Option<(u32, u32)> {
        let shared = cameras
            .iter()
            .filter(|other| other.facing != camera.facing)
            .fold(camera.sizes.clone(), |sizes, other| {
                common_sizes(&sizes, &other.sizes)
            });
        let sizes = if shared.is_empty() {
            &camera.sizes
        } else {
            &shared
        };
        choose_size(sizes, (config.width, config.height))
    }

    impl Running {
        /// Opens `camera` and starts capturing into the encoder (and the preview). The encoder
        /// stays unless the camera cannot capture at its size.
        fn open_camera(
            &mut self,
            camera: CameraInfo,
            display: Rotation,
            preview: Option<&Window>,
            health: &Arc<CameraHealth>,
        ) -> Result<(), VideoError> {
            self.session = None;
            self.device = None;
            if !camera.sizes.contains(&self.encoder.size()) {
                let size = capture_size(&self.cameras, &camera, &self.config)
                    .ok_or(VideoError::Unsupported)?;
                self.encoder =
                    Encoder::start(size, &self.config, self.out.clone(), self.shared.clone())?;
            }
            self.shared
                .set_rotation(frame_rotation(camera.orientation, camera.facing, display));
            let device = self.manager.open(&camera, health)?;
            let windows = session_windows(&self.encoder, preview);
            let fps = choose_fps_range(&camera.fps_ranges, self.config.fps);
            self.session = Some(CaptureSession::start(&device, windows, fps)?);
            self.device = Some(device);
            self.camera = camera;
            Ok(())
        }
    }

    impl VideoSource for CameraSource {
        /// Opens the camera: [`VideoError::PermissionDenied`] without the `CAMERA` permission
        /// (or with the camera disabled by policy), [`VideoError::NoCamera`] without a camera
        /// facing that way, [`VideoError::Unsupported`] before Android 8.0.
        fn start(
            &mut self,
            config: VideoConfig,
            facing: Facing,
            out: FrameSender,
        ) -> Result<(), VideoError> {
            self.stop()?;
            self.health.lost.store(false, Ordering::Release);
            self.health.error.store(0, Ordering::Relaxed);
            let manager = Manager::new()?;
            let cameras = manager.cameras()?;
            let camera = cameras
                .iter()
                .find(|camera| camera.facing == facing)
                .cloned()
                .ok_or(VideoError::NoCamera)?;
            let size = capture_size(&cameras, &camera, &config).ok_or(VideoError::Unsupported)?;
            let shared = Arc::new(EncoderShared::default());
            let encoder = Encoder::start(size, &config, out.clone(), shared.clone())?;
            let mut running = Running {
                session: None,
                device: None,
                encoder,
                manager,
                cameras,
                camera: camera.clone(),
                config,
                out,
                shared,
            };
            running.open_camera(camera, self.display, self.preview.as_ref(), &self.health)?;
            self.running = Some(running);
            Ok(())
        }

        fn stop(&mut self) -> Result<(), VideoError> {
            self.running = None;
            Ok(())
        }

        fn request_keyframe(&mut self) {
            if let Some(running) = &self.running {
                running.encoder.request_keyframe();
            }
        }

        fn set_bitrate(&mut self, bps: u32) {
            if let Some(running) = &mut self.running {
                running.config.bitrate_bps = clamp_bitrate(bps);
                running.encoder.set_bitrate(bps);
            }
        }

        fn switch_camera(&mut self, facing: Facing) -> Result<(), VideoError> {
            let Some(running) = &mut self.running else {
                return Ok(());
            };
            if running.camera.facing == facing {
                return Ok(());
            }
            let camera = running
                .cameras
                .iter()
                .find(|camera| camera.facing == facing)
                .cloned()
                .ok_or(VideoError::NoCamera)?;
            running.open_camera(camera, self.display, self.preview.as_ref(), &self.health)?;
            running.encoder.request_keyframe();
            Ok(())
        }
    }

    impl Drop for CameraSource {
        fn drop(&mut self) {
            // Closes the camera before `health`, which its callbacks point to, goes.
            self.running = None;
        }
    }

    // ---------------------------------------------------------------------------------------
    // DisplaySink
    // ---------------------------------------------------------------------------------------

    /// The pipeline's view of a sink's need for a keyframe: poll [`KeyframeRequest::is_needed`]
    /// and send a PLI (rate limited by the pipeline) while it is `true`.
    #[derive(Debug, Clone, Default)]
    pub struct KeyframeRequest(Arc<AtomicBool>);

    impl KeyframeRequest {
        /// Whether the sink cannot decode until a keyframe arrives (none yet, a lost frame, a
        /// decoder error or reset) or needs one to apply a new rotation.
        pub fn is_needed(&self) -> bool {
            self.0.load(Ordering::Relaxed)
        }

        fn set(&self, needed: bool) {
            self.0.store(needed, Ordering::Relaxed);
        }
    }

    /// Where a decoder renders.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Target {
        /// The app's surface.
        App,
        /// The placeholder surface: frames are decoded and dropped.
        Placeholder,
        /// No surface (Android 7 without an app surface): frames are decoded and dropped, and
        /// the decoder cannot move to a surface later.
        Buffers,
    }

    /// A surface nobody shows, for the decoder while the app has none (`AImageReader`,
    /// Android 8.0). The decoder never renders to it: it only keeps the decoder in surface
    /// mode, so it can move back to the app's surface without a reset.
    struct Placeholder {
        media: &'static Media,
        reader: NonNull<AImageReader>,
        window: NonNull<ANativeWindow>,
    }

    // SAFETY: an image reader may be used from any thread; only its owner touches it.
    unsafe impl Send for Placeholder {}

    impl Placeholder {
        fn new(media: &'static Media, width: u32, height: u32) -> Option<Self> {
            let new_reader = media.new_image_reader?;
            let mut reader = ptr::null_mut();
            // SAFETY: `reader` is a valid place.
            let status = unsafe {
                new_reader(
                    i32::try_from(width).ok()?,
                    i32::try_from(height).ok()?,
                    IMAGE_FORMAT_PRIVATE,
                    USAGE_GPU_SAMPLED_IMAGE,
                    2,
                    &mut reader,
                )
            };
            let reader = NonNull::new(reader).filter(|_| status == AMEDIA_OK)?;
            let mut window = ptr::null_mut();
            // SAFETY: the reader is live; the window it gives is owned by it.
            let status = unsafe { (media.image_reader_window)(reader.as_ptr(), &mut window) };
            let placeholder_window = NonNull::new(window).filter(|_| status == AMEDIA_OK);
            let Some(window) = placeholder_window else {
                // SAFETY: created above, deleted once.
                unsafe { (media.image_reader_delete)(reader.as_ptr()) };
                return None;
            };
            Some(Self {
                media,
                reader,
                window,
            })
        }
    }

    impl Drop for Placeholder {
        fn drop(&mut self) {
            // SAFETY: created in `new`, deleted once, after the decoder rendering to it.
            unsafe { (self.media.image_reader_delete)(self.reader.as_ptr()) };
        }
    }

    /// A running hardware decoder with the thread that renders its output. Dropping it stops
    /// the thread, then the codec.
    struct Decoder {
        codec: Arc<Codec>,
        target: Target,
        /// Whether the output thread renders (app surface) or drops (placeholder, buffers).
        render: Arc<AtomicBool>,
        failed: Arc<AtomicBool>,
        stop: Arc<AtomicBool>,
        thread: Option<JoinHandle<()>>,
        /// Dropped after the codec.
        placeholder: Option<Placeholder>,
        media: &'static Media,
        /// The coded size, for a placeholder made later.
        size: (u32, u32),
    }

    impl Decoder {
        fn start(
            params: &StreamParams,
            surface: Option<&Window>,
            keyframe: &KeyframeRequest,
        ) -> Result<Self, VideoError> {
            let media = Media::get()?;
            let (target, placeholder) = match surface {
                Some(_) => (Target::App, None),
                None => match Placeholder::new(media, params.width, params.height) {
                    Some(placeholder) => (Target::Placeholder, Some(placeholder)),
                    None => (Target::Buffers, None),
                },
            };
            let window = match (surface, &placeholder) {
                (Some(surface), _) => surface.as_ptr(),
                (None, Some(placeholder)) => placeholder.window.as_ptr(),
                (None, None) => ptr::null_mut(),
            };
            let codec = Codec::create(media, false)?;
            codec.configure(&Format::new(media, &decoder_format(params))?, window, 0)?;
            codec.start()?;
            let codec = Arc::new(codec);
            let render = Arc::new(AtomicBool::new(target == Target::App));
            let failed = Arc::new(AtomicBool::new(false));
            let stop = Arc::new(AtomicBool::new(false));
            let thread = thread::Builder::new()
                .name("ft-video-decode".to_owned())
                .spawn({
                    let codec = codec.clone();
                    let render = render.clone();
                    let failed = failed.clone();
                    let stop = stop.clone();
                    let keyframe = keyframe.clone();
                    move || {
                        if !render_decoder(&codec, &render, &stop) {
                            failed.store(true, Ordering::Relaxed);
                            keyframe.set(true);
                        }
                    }
                })
                .map_err(|error| VideoError::Backend(format!("decoder thread: {error}")))?;
            Ok(Self {
                codec,
                target,
                render,
                failed,
                stop,
                thread: Some(thread),
                placeholder,
                media,
                size: (params.width, params.height),
            })
        }

        /// Moves the output to the app's surface, or to the placeholder with `None`, without a
        /// reset. `false` if the decoder cannot and must be rebuilt.
        fn retarget(&mut self, surface: Option<&Window>) -> bool {
            if self.target == Target::Buffers {
                return false;
            }
            match surface {
                Some(surface) => {
                    if self.codec.set_output_surface(surface.as_ptr()).is_err() {
                        return false;
                    }
                    self.target = Target::App;
                    self.render.store(true, Ordering::Relaxed);
                    true
                }
                None => {
                    // Stop rendering first: the old surface is still alive until this returns.
                    self.render.store(false, Ordering::Relaxed);
                    if self.placeholder.is_none() {
                        self.placeholder = Placeholder::new(self.media, self.size.0, self.size.1);
                    }
                    let Some(placeholder) = &self.placeholder else {
                        return false;
                    };
                    if self
                        .codec
                        .set_output_surface(placeholder.window.as_ptr())
                        .is_err()
                    {
                        return false;
                    }
                    self.target = Target::Placeholder;
                    true
                }
            }
        }
    }

    impl Drop for Decoder {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    /// The decoder's output loop: renders each frame to the surface as soon as it is decoded
    /// (or drops it without a surface). `false` if the codec failed.
    fn render_decoder(codec: &Codec, render: &AtomicBool, stop: &AtomicBool) -> bool {
        let mut info = BufferInfo::default();
        while !stop.load(Ordering::Relaxed) {
            let index = codec.dequeue_output(&mut info, OUTPUT_TIMEOUT_US);
            match usize::try_from(index) {
                Ok(index) => codec.release_output(index, render.load(Ordering::Relaxed)),
                Err(_)
                    if matches!(
                        index,
                        INFO_TRY_AGAIN_LATER
                            | INFO_OUTPUT_FORMAT_CHANGED
                            | INFO_OUTPUT_BUFFERS_CHANGED
                    ) => {}
                Err(_) => return false,
            }
        }
        true
    }

    /// The remote video: H.264 decoded by the hardware decoder straight onto the app's
    /// surface. See the [module documentation](super) for the app's side.
    pub struct DisplaySink {
        /// Dropped before `surface`, which it may render to.
        decoder: Option<Decoder>,
        surface: Option<Window>,
        state: SinkState,
        keyframe: KeyframeRequest,
        running: bool,
    }

    impl Default for DisplaySink {
        fn default() -> Self {
            Self::new()
        }
    }

    impl DisplaySink {
        pub fn new() -> Self {
            Self {
                decoder: None,
                surface: None,
                state: SinkState::default(),
                keyframe: KeyframeRequest::default(),
                running: false,
            }
        }

        /// Renders into `window` (from `ANativeWindow_fromSurface` on the remote video
        /// `SurfaceView`'s `Surface`), or stops rendering with `None`, which the app must call
        /// in `surfaceDestroyed`. The decoder moves between surfaces without a reset when the
        /// platform allows it (Android 8.0 and later); otherwise it is rebuilt at the next
        /// keyframe and [`KeyframeRequest::is_needed`] turns on. The sink takes a reference of
        /// its own: the caller may release its reference right after.
        ///
        /// # Safety
        ///
        /// `window` is `None` or a live `ANativeWindow`.
        pub unsafe fn set_surface(&mut self, window: Option<NonNull<ANativeWindow>>) {
            // SAFETY: live (caller).
            let surface = window.map(|window| unsafe { Window::acquire(window) });
            if let Some(decoder) = &mut self.decoder
                && !decoder.retarget(surface.as_ref())
            {
                self.drop_decoder();
            }
            // The old surface is released only now, after the decoder left it.
            self.surface = surface;
        }

        /// The handle the pipeline polls to ask the sender for a keyframe.
        pub fn keyframe_request(&self) -> KeyframeRequest {
            self.keyframe.clone()
        }

        /// Whether a keyframe is needed; see [`KeyframeRequest::is_needed`].
        pub fn keyframe_needed(&self) -> bool {
            self.keyframe.is_needed()
        }

        /// The size of the video as shown (rotated upright), once a keyframe has been decoded,
        /// for the app to lay the view out with the right aspect ratio.
        pub fn video_size(&self) -> Option<(u32, u32)> {
            self.state
                .configured
                .as_ref()
                .map(StreamParams::displayed_size)
        }

        fn drop_decoder(&mut self) {
            self.decoder = None;
            self.state.on_decoder_lost();
            self.keyframe.set(true);
        }

        fn decode(&mut self, frame: &EncodedFrame) -> Result<(), VideoError> {
            let Some(decoder) = &self.decoder else {
                self.drop_decoder();
                return Ok(());
            };
            let time_us = u64::try_from(frame.timestamp.as_micros()).unwrap_or(u64::MAX);
            match decoder.codec.queue_input(&frame.data, time_us) {
                Ok(true) => Ok(()),
                Ok(false) => {
                    self.state.on_frame_lost();
                    Ok(())
                }
                Err(_) => {
                    self.drop_decoder();
                    Ok(())
                }
            }
        }
    }

    impl VideoSink for DisplaySink {
        fn start(&mut self) -> Result<(), VideoError> {
            Media::get()?;
            self.decoder = None;
            self.state = SinkState::default();
            self.keyframe.set(self.state.keyframe_needed());
            self.running = true;
            Ok(())
        }

        /// Decodes `frame`. Decoder errors do not fail the call: the decoder is rebuilt at the
        /// next keyframe and [`KeyframeRequest::is_needed`] turns on. Fails only before
        /// `start`, or when a decoder cannot be built at all.
        fn push(&mut self, frame: EncodedFrame) -> Result<(), VideoError> {
            if !self.running {
                return Err(VideoError::Backend("display sink not started".to_owned()));
            }
            if self
                .decoder
                .as_ref()
                .is_some_and(|decoder| decoder.failed.load(Ordering::Relaxed))
            {
                self.drop_decoder();
            }
            let result = match self.state.on_frame(&frame) {
                SinkStep::Drop => Ok(()),
                SinkStep::Decode => self.decode(&frame),
                SinkStep::Configure(params) => {
                    self.decoder = None;
                    match Decoder::start(&params, self.surface.as_ref(), &self.keyframe) {
                        Ok(decoder) => {
                            self.decoder = Some(decoder);
                            self.decode(&frame)
                        }
                        Err(error) => {
                            self.drop_decoder();
                            Err(error)
                        }
                    }
                }
            };
            self.keyframe.set(self.state.keyframe_needed());
            result
        }

        fn stop(&mut self) -> Result<(), VideoError> {
            self.decoder = None;
            self.state = SinkState::default();
            self.keyframe.set(false);
            self.running = false;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const SPS: [u8; 4] = [0x67, 0x42, 0xc0, 0x1e];
    const PPS: [u8; 3] = [0x68, 0xcb, 0x83];
    const IDR: [u8; 3] = [0x65, 0x88, 0x84];
    const DELTA: [u8; 3] = [0x41, 0x9a, 0x02];

    /// NAL units in Annex-B, each behind a 4-byte start code.
    fn annex_b(nals: &[&[u8]]) -> Vec<u8> {
        nals.iter()
            .flat_map(|nal| START_CODE.iter().chain(nal.iter()))
            .copied()
            .collect()
    }

    #[test]
    fn nal_units_are_split_on_both_start_code_lengths() {
        let data = [
            0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 4,
        ];
        assert_eq!(
            nal_units(&data),
            [&[0x67, 1, 2][..], &[0x68, 3][..], &[0x65, 4][..]]
        );
        assert_eq!(nal_type(&[0x65, 4]), Some(NAL_IDR));
        assert_eq!(nal_type(&[]), None);
    }

    #[test]
    fn trailing_zeros_and_leading_garbage_are_not_part_of_a_nal_unit() {
        let data = [9, 9, 0, 0, 1, 0x41, 7, 0, 0, 0, 0, 1, 0x41, 8, 0];
        assert_eq!(nal_units(&data), [&[0x41, 7][..], &[0x41, 8][..]]);
        assert!(nal_units(&[1, 2, 3]).is_empty());
        assert!(nal_units(&[]).is_empty());
    }

    #[test]
    fn annex_b_is_kept_and_avcc_is_converted() {
        let annex = annex_b(&[&SPS, &IDR]);
        assert_eq!(to_annex_b(&annex), Some(annex.clone()));
        assert_eq!(to_annex_b(&[0, 0, 1, 0x41]), Some(vec![0, 0, 1, 0x41]));

        let avcc = [
            0, 0, 0, 4, 0x67, 0x42, 0xc0, 0x1e, 0, 0, 0, 3, 0x65, 0x88, 0x84,
        ];
        assert_eq!(to_annex_b(&avcc), Some(annex));
    }

    #[test]
    fn buffers_that_are_neither_annex_b_nor_avcc_are_refused() {
        assert_eq!(to_annex_b(&[]), None);
        // A length running past the end, and a zero length.
        assert_eq!(to_annex_b(&[0, 0, 0, 9, 0x65, 1]), None);
        assert_eq!(to_annex_b(&[0, 0, 0, 0, 0, 0, 0, 1, 0x65]), None);
    }

    #[test]
    fn codec_config_is_kept_and_put_in_front_of_the_next_keyframe() {
        let mut packager = AccessUnitPackager::default();
        let config = annex_b(&[&SPS, &PPS]);
        assert_eq!(packager.package(&config, BUFFER_FLAG_CODEC_CONFIG), None);

        let keyframe = packager.package(&annex_b(&[&IDR]), BUFFER_FLAG_KEY_FRAME);
        assert_eq!(keyframe, Some((annex_b(&[&SPS, &PPS, &IDR]), true)));
        // Every keyframe, not only the first.
        let again = packager.package(&annex_b(&[&IDR]), BUFFER_FLAG_KEY_FRAME);
        assert_eq!(again, Some((annex_b(&[&SPS, &PPS, &IDR]), true)));
    }

    #[test]
    fn delta_frames_go_out_as_they_are() {
        let mut packager = AccessUnitPackager::default();
        packager.package(&annex_b(&[&SPS, &PPS]), BUFFER_FLAG_CODEC_CONFIG);
        assert_eq!(
            packager.package(&annex_b(&[&DELTA]), 0),
            Some((annex_b(&[&DELTA]), false))
        );
    }

    #[test]
    fn a_keyframe_that_carries_its_parameter_sets_is_not_given_them_twice() {
        let mut packager = AccessUnitPackager::default();
        packager.package(&annex_b(&[&SPS, &PPS]), BUFFER_FLAG_CODEC_CONFIG);
        let newer_sps = [0x67, 0x42, 0xc0, 0x1f];
        let inline = annex_b(&[&newer_sps, &PPS, &IDR]);
        assert_eq!(
            packager.package(&inline, BUFFER_FLAG_KEY_FRAME),
            Some((inline, true))
        );
        // The in-band ones replace the cached ones.
        assert_eq!(
            packager.package(&annex_b(&[&IDR]), BUFFER_FLAG_KEY_FRAME),
            Some((annex_b(&[&newer_sps, &PPS, &IDR]), true))
        );
    }

    #[test]
    fn a_new_codec_config_replaces_the_old_one() {
        let mut packager = AccessUnitPackager::default();
        packager.package(&annex_b(&[&SPS, &PPS]), BUFFER_FLAG_CODEC_CONFIG);
        let newer_sps = [0x67, 0x42, 0xc0, 0x1f];
        packager.package(&annex_b(&[&newer_sps, &PPS]), BUFFER_FLAG_CODEC_CONFIG);
        assert_eq!(
            packager.package(&annex_b(&[&IDR]), BUFFER_FLAG_KEY_FRAME),
            Some((annex_b(&[&newer_sps, &PPS, &IDR]), true))
        );
    }

    #[test]
    fn an_idr_slice_is_a_keyframe_even_without_the_flag() {
        let mut packager = AccessUnitPackager::default();
        packager.package(&annex_b(&[&SPS, &PPS]), BUFFER_FLAG_CODEC_CONFIG);
        assert_eq!(
            packager.package(&annex_b(&[&IDR]), 0),
            Some((annex_b(&[&SPS, &PPS, &IDR]), true))
        );
    }

    #[test]
    fn avcc_output_is_sent_as_annex_b() {
        let mut packager = AccessUnitPackager::default();
        let avcc = [0, 0, 0, 3, 0x41, 0x9a, 0x02];
        assert_eq!(
            packager.package(&avcc, 0),
            Some((annex_b(&[&DELTA]), false))
        );
    }

    #[test]
    fn empty_or_unreadable_buffers_send_nothing() {
        let mut packager = AccessUnitPackager::default();
        assert_eq!(packager.package(&[], 0), None);
        assert_eq!(packager.package(&[1, 2, 3], BUFFER_FLAG_KEY_FRAME), None);
    }

    // Real SPS and PPS from x264 (baseline 640x480, 320x180 with cropping, 1280x720; high
    // profile 640x360).
    const SPS_640X480: [u8; 24] = [
        0x67, 0x42, 0xc0, 0x1e, 0xd9, 0x00, 0xa0, 0x3d, 0xb0, 0x11, 0x00, 0x00, 0x03, 0x00, 0x01,
        0x00, 0x00, 0x03, 0x00, 0x3c, 0x0f, 0x16, 0x2e, 0x48,
    ];
    const SPS_320X180: [u8; 25] = [
        0x67, 0x42, 0xc0, 0x0d, 0xd9, 0x01, 0x41, 0x9f, 0x9f, 0x01, 0x10, 0x00, 0x00, 0x03, 0x00,
        0x10, 0x00, 0x00, 0x03, 0x03, 0xc0, 0xf1, 0x42, 0xa4, 0x80,
    ];
    const SPS_1280X720: [u8; 25] = [
        0x67, 0x42, 0xc0, 0x1f, 0xd9, 0x00, 0x50, 0x05, 0xbb, 0x01, 0x10, 0x00, 0x00, 0x03, 0x00,
        0x10, 0x00, 0x00, 0x03, 0x03, 0xc0, 0xf1, 0x83, 0x24, 0x80,
    ];
    const SPS_HIGH_640X360: [u8; 26] = [
        0x67, 0x64, 0x00, 0x1e, 0xac, 0xd9, 0x40, 0xa0, 0x2f, 0xf9, 0x70, 0x11, 0x00, 0x00, 0x03,
        0x00, 0x01, 0x00, 0x00, 0x03, 0x00, 0x3c, 0x0f, 0x16, 0x2d, 0x96,
    ];
    const PPS_X264: [u8; 5] = [0x68, 0xcb, 0x83, 0xcb, 0x20];

    #[test]
    fn emulation_prevention_bytes_are_removed() {
        assert_eq!(
            unescape(&[0x67, 0, 0, 3, 1, 0, 0, 3]),
            [0x67, 0, 0, 1, 0, 0]
        );
        // Only after two zeros.
        assert_eq!(unescape(&[0, 3, 0, 0, 3, 0, 3]), [0, 3, 0, 0, 0, 3]);
    }

    #[test]
    fn the_sps_gives_the_picture_size() {
        assert_eq!(sps_dimensions(&SPS_640X480), Some((640, 480)));
        assert_eq!(sps_dimensions(&SPS_1280X720), Some((1280, 720)));
    }

    #[test]
    fn the_sps_cropping_is_applied() {
        assert_eq!(sps_dimensions(&SPS_320X180), Some((320, 180)));
    }

    #[test]
    fn a_high_profile_sps_is_read_too() {
        assert_eq!(sps_dimensions(&SPS_HIGH_640X360), Some((640, 360)));
    }

    #[test]
    fn a_truncated_or_foreign_nal_unit_has_no_size() {
        assert_eq!(sps_dimensions(&SPS_640X480[..6]), None);
        assert_eq!(sps_dimensions(&[]), None);
        assert_eq!(sps_dimensions(&PPS_X264), None);
    }

    fn keyframe(sps: &[u8], rotation: Rotation) -> EncodedFrame {
        EncodedFrame {
            data: annex_b(&[sps, &PPS_X264, &IDR]),
            keyframe: true,
            timestamp: Duration::ZERO,
            rotation,
        }
    }

    fn delta(rotation: Rotation) -> EncodedFrame {
        EncodedFrame {
            data: annex_b(&[&DELTA]),
            keyframe: false,
            timestamp: Duration::from_millis(33),
            rotation,
        }
    }

    fn params(sps: &[u8], width: u32, height: u32, rotation: Rotation) -> StreamParams {
        StreamParams {
            sps: sps.to_vec(),
            pps: PPS_X264.to_vec(),
            width,
            height,
            rotation,
        }
    }

    #[test]
    fn stream_params_come_from_the_keyframe() {
        let frame = keyframe(&SPS_640X480, Rotation::Deg90);
        assert_eq!(
            StreamParams::from_keyframe(&frame),
            Some(params(&SPS_640X480, 640, 480, Rotation::Deg90))
        );
        assert_eq!(StreamParams::from_keyframe(&delta(Rotation::Deg0)), None);
    }

    #[test]
    fn a_quarter_turn_swaps_the_displayed_size() {
        let size = |rotation| params(&SPS_640X480, 640, 480, rotation).displayed_size();
        assert_eq!(size(Rotation::Deg0), (640, 480));
        assert_eq!(size(Rotation::Deg90), (480, 640));
        assert_eq!(size(Rotation::Deg180), (640, 480));
        assert_eq!(size(Rotation::Deg270), (480, 640));
    }

    #[test]
    fn the_sink_waits_for_a_keyframe_and_configures_from_it() {
        let mut state = SinkState::default();
        assert!(state.keyframe_needed());
        assert_eq!(state.on_frame(&delta(Rotation::Deg0)), SinkStep::Drop);

        let first = keyframe(&SPS_640X480, Rotation::Deg0);
        assert_eq!(
            state.on_frame(&first),
            SinkStep::Configure(params(&SPS_640X480, 640, 480, Rotation::Deg0))
        );
        assert!(!state.keyframe_needed());
        assert_eq!(state.on_frame(&delta(Rotation::Deg0)), SinkStep::Decode);
        // The same parameters again: no new decoder.
        assert_eq!(state.on_frame(&first), SinkStep::Decode);
    }

    #[test]
    fn a_keyframe_without_a_readable_sps_is_dropped() {
        let mut state = SinkState::default();
        let mut broken = keyframe(&SPS_640X480[..6], Rotation::Deg0);
        assert_eq!(state.on_frame(&broken), SinkStep::Drop);
        broken.data = annex_b(&[&IDR]);
        assert_eq!(state.on_frame(&broken), SinkStep::Drop);
        assert!(state.keyframe_needed());
    }

    #[test]
    fn a_lost_frame_drops_deltas_until_the_next_keyframe() {
        let mut state = SinkState::default();
        state.on_frame(&keyframe(&SPS_640X480, Rotation::Deg0));
        state.on_frame_lost();
        assert!(state.keyframe_needed());
        assert_eq!(state.on_frame(&delta(Rotation::Deg0)), SinkStep::Drop);
        // The decoder is still good: no reconfiguration.
        assert_eq!(
            state.on_frame(&keyframe(&SPS_640X480, Rotation::Deg0)),
            SinkStep::Decode
        );
        assert!(!state.keyframe_needed());
    }

    #[test]
    fn a_lost_decoder_is_rebuilt_at_the_next_keyframe() {
        let mut state = SinkState::default();
        state.on_frame(&keyframe(&SPS_640X480, Rotation::Deg0));
        state.on_decoder_lost();
        assert!(state.keyframe_needed());
        assert_eq!(state.on_frame(&delta(Rotation::Deg0)), SinkStep::Drop);
        assert_eq!(
            state.on_frame(&keyframe(&SPS_640X480, Rotation::Deg0)),
            SinkStep::Configure(params(&SPS_640X480, 640, 480, Rotation::Deg0))
        );
    }

    #[test]
    fn a_new_size_reconfigures_at_its_keyframe() {
        let mut state = SinkState::default();
        state.on_frame(&keyframe(&SPS_640X480, Rotation::Deg0));
        assert_eq!(
            state.on_frame(&keyframe(&SPS_1280X720, Rotation::Deg0)),
            SinkStep::Configure(params(&SPS_1280X720, 1280, 720, Rotation::Deg0))
        );
        assert!(!state.keyframe_needed());
    }

    #[test]
    fn a_turn_keeps_decoding_and_asks_for_a_keyframe_to_apply_it() {
        let mut state = SinkState::default();
        state.on_frame(&keyframe(&SPS_640X480, Rotation::Deg90));
        // The old rotation stays until a keyframe can rebuild the decoder.
        assert_eq!(state.on_frame(&delta(Rotation::Deg0)), SinkStep::Decode);
        assert!(state.keyframe_needed());
        assert_eq!(state.on_frame(&delta(Rotation::Deg0)), SinkStep::Decode);
        assert_eq!(
            state.on_frame(&keyframe(&SPS_640X480, Rotation::Deg0)),
            SinkStep::Configure(params(&SPS_640X480, 640, 480, Rotation::Deg0))
        );
        assert!(!state.keyframe_needed());
    }

    #[test]
    fn a_turn_back_before_the_keyframe_needs_nothing() {
        let mut state = SinkState::default();
        state.on_frame(&keyframe(&SPS_640X480, Rotation::Deg90));
        state.on_frame(&delta(Rotation::Deg0));
        assert_eq!(state.on_frame(&delta(Rotation::Deg90)), SinkStep::Decode);
        assert!(!state.keyframe_needed());
    }

    #[test]
    fn a_back_camera_turns_by_its_mounting_minus_the_screen() {
        let back = |sensor, display| frame_rotation(sensor, Facing::Back, display);
        assert_eq!(back(90, Rotation::Deg0), Rotation::Deg90);
        assert_eq!(back(90, Rotation::Deg90), Rotation::Deg0);
        assert_eq!(back(90, Rotation::Deg270), Rotation::Deg180);
        assert_eq!(back(0, Rotation::Deg90), Rotation::Deg270);
    }

    #[test]
    fn a_front_camera_turns_by_its_mounting_plus_the_screen() {
        let front = |sensor, display| frame_rotation(sensor, Facing::Front, display);
        assert_eq!(front(270, Rotation::Deg0), Rotation::Deg270);
        assert_eq!(front(270, Rotation::Deg90), Rotation::Deg0);
        assert_eq!(front(90, Rotation::Deg180), Rotation::Deg270);
    }

    #[test]
    fn odd_sensor_orientations_are_normalised() {
        assert_eq!(
            frame_rotation(-90, Facing::Back, Rotation::Deg0),
            Rotation::Deg270
        );
        assert_eq!(
            frame_rotation(450, Facing::Back, Rotation::Deg0),
            Rotation::Deg90
        );
        assert_eq!(
            frame_rotation(100, Facing::Back, Rotation::Deg0),
            Rotation::Deg90
        );
    }

    #[test]
    fn lens_facings_map_to_cameras() {
        assert_eq!(lens_facing(0), Some(Facing::Front));
        assert_eq!(lens_facing(1), Some(Facing::Back));
        assert_eq!(lens_facing(2), None);
    }

    #[test]
    fn output_sizes_are_read_for_one_format() {
        let configurations = [
            0x22, 1280, 720, 0, //
            0x23, 640, 480, 0, //
            0x22, 640, 480, 1, // input
            0x22, 640, 480, 0, //
            0x22, 0, 480, 0, //
            0x22, 320, // truncated
        ];
        assert_eq!(
            output_sizes(&configurations, 0x22),
            [(1280, 720), (640, 480)]
        );
    }

    #[test]
    fn common_sizes_keep_the_first_order() {
        assert_eq!(
            common_sizes(
                &[(1280, 720), (640, 480), (320, 240)],
                &[(320, 240), (640, 480)]
            ),
            [(640, 480), (320, 240)]
        );
    }

    #[test]
    fn the_size_asked_for_is_taken_when_there() {
        let sizes = [(1920, 1080), (640, 480), (320, 240)];
        assert_eq!(choose_size(&sizes, (640, 480)), Some((640, 480)));
    }

    #[test]
    fn otherwise_the_closest_size_with_the_same_shape() {
        let sizes = [(1920, 1080), (1280, 720), (800, 600), (320, 240)];
        assert_eq!(choose_size(&sizes, (640, 480)), Some((800, 600)));
    }

    #[test]
    fn otherwise_the_closest_size() {
        let sizes = [(1920, 1080), (960, 540), (640, 360)];
        assert_eq!(choose_size(&sizes, (640, 480)), Some((640, 360)));
        assert_eq!(choose_size(&[], (640, 480)), None);
    }

    #[test]
    fn the_frame_rate_range_reaches_the_rate_and_lets_exposure_stretch() {
        let ranges = [15, 15, 7, 30, 15, 30, 30, 30, 24, 24];
        assert_eq!(choose_fps_range(&ranges, 30), Some([15, 30]));
        assert_eq!(choose_fps_range(&[7, 30, 30, 30], 30), Some([7, 30]));
        assert_eq!(choose_fps_range(&ranges, 15), Some([15, 15]));
        assert_eq!(choose_fps_range(&[], 30), None);
    }

    #[test]
    fn legacy_ranges_in_thousandths_are_understood() {
        let ranges = [15_000, 15_000, 15_000, 30_000, 30_000, 30_000];
        assert_eq!(choose_fps_range(&ranges, 30), Some([15_000, 30_000]));
    }

    #[test]
    fn a_camera_refused_by_permission_or_policy_is_permission_denied() {
        assert_eq!(camera_error(-10013), VideoError::PermissionDenied);
        assert_eq!(camera_error(-10012), VideoError::PermissionDenied);
        assert!(matches!(camera_error(-10010), VideoError::Backend(_)));
    }

    #[test]
    fn the_encoder_is_asked_for_real_time_baseline_h264_from_a_surface() {
        let config = VideoConfig::default();
        let formats = encoder_formats(640, 480, &config);
        assert_eq!(formats.len(), 3);
        assert_eq!(
            formats[0],
            [
                (KEY_MIME, FormatValue::Str(MIME_AVC)),
                (KEY_WIDTH, FormatValue::Int(640)),
                (KEY_HEIGHT, FormatValue::Int(480)),
                (KEY_COLOR_FORMAT, FormatValue::Int(COLOR_FORMAT_SURFACE)),
                (KEY_BIT_RATE, FormatValue::Int(800_000)),
                (KEY_FRAME_RATE, FormatValue::Int(30)),
                (KEY_I_FRAME_INTERVAL, FormatValue::Int(2)),
                (KEY_PRIORITY, FormatValue::Int(0)),
                (KEY_PREPEND_HEADER_TO_SYNC_FRAMES, FormatValue::Int(1)),
                (KEY_BITRATE_MODE, FormatValue::Int(BITRATE_MODE_CBR)),
                (KEY_PROFILE, FormatValue::Int(AVC_PROFILE_BASELINE)),
                (KEY_LEVEL, FormatValue::Int(AVC_LEVEL_31)),
            ]
        );
        let mode = |format: &[FormatEntry]| {
            format
                .iter()
                .find(|(key, _)| *key == KEY_BITRATE_MODE)
                .map(|(_, value)| value.clone())
        };
        assert_eq!(mode(&formats[1]), Some(FormatValue::Int(BITRATE_MODE_VBR)));
        assert_eq!(formats[1].len(), formats[0].len());
        assert!(!formats[2].iter().any(|(key, _)| *key == KEY_PROFILE));
        assert!(!formats[2].iter().any(|(key, _)| *key == KEY_LEVEL));
    }

    #[test]
    fn the_encoder_bitrate_is_clamped() {
        let config = VideoConfig {
            bitrate_bps: 10_000_000,
            ..VideoConfig::default()
        };
        let formats = encoder_formats(640, 480, &config);
        assert!(formats[0].contains(&(KEY_BIT_RATE, FormatValue::Int(2_500_000))));
        assert_eq!(
            bitrate_parameters(1),
            [(PARAMETER_VIDEO_BITRATE, FormatValue::Int(150_000))]
        );
        assert_eq!(
            keyframe_parameters(),
            [(PARAMETER_REQUEST_SYNC_FRAME, FormatValue::Int(0))]
        );
    }

    #[test]
    fn the_decoder_is_configured_from_the_keyframe() {
        let format = decoder_format(&params(&SPS_640X480, 640, 480, Rotation::Deg270));
        let mut csd_0 = START_CODE.to_vec();
        csd_0.extend_from_slice(&SPS_640X480);
        let mut csd_1 = START_CODE.to_vec();
        csd_1.extend_from_slice(&PPS_X264);
        assert_eq!(
            format,
            [
                (KEY_MIME, FormatValue::Str(MIME_AVC)),
                (KEY_WIDTH, FormatValue::Int(640)),
                (KEY_HEIGHT, FormatValue::Int(480)),
                (KEY_ROTATION, FormatValue::Int(270)),
                (KEY_PRIORITY, FormatValue::Int(0)),
                (KEY_LOW_LATENCY, FormatValue::Int(1)),
                (KEY_CSD_0, FormatValue::Buffer(csd_0)),
                (KEY_CSD_1, FormatValue::Buffer(csd_1)),
            ]
        );
    }

    #[test]
    fn a_needed_keyframe_is_asked_for_once() {
        let mut requests = KeyframeRequests::default();
        assert!(!requests.after_frame(false, false));
        assert!(requests.after_frame(false, true));
        assert!(!requests.after_frame(false, true));
        // The keyframe came: the next need asks again.
        assert!(!requests.after_frame(true, false));
        assert!(requests.after_frame(false, true));
    }

    #[test]
    fn a_request_that_brings_no_keyframe_is_repeated() {
        let mut requests = KeyframeRequests::default();
        assert!(requests.after_frame(false, true));
        let asked: Vec<bool> = (0..KEYFRAME_RETRY_FRAMES)
            .map(|_| requests.after_frame(false, true))
            .collect();
        assert_eq!(asked.iter().filter(|&&asked| asked).count(), 1);
        assert_eq!(asked.last(), Some(&true));
    }

    #[test]
    fn a_request_from_the_engine_counts_as_pending() {
        let mut requests = KeyframeRequests::default();
        requests.requested();
        assert!(!requests.after_frame(false, true));
    }

    // On the device: `cargo test --target aarch64-linux-android --lib --no-run`, push the test
    // binary to /data/local/tmp and run it with `--ignored video::android --test-threads=1`
    // from `adb shell`.
    #[cfg(target_os = "android")]
    mod device {
        use super::super::device::{EncoderShared, Window};
        use super::super::{ANativeWindow, CameraSource, DisplaySink, device::Encoder};
        use super::*;
        use crate::video::{VideoSink, VideoSource, frame_channel};
        use std::ffi::c_void;
        use std::ptr::{self, NonNull};
        use std::sync::Arc;
        use std::thread::sleep;

        #[repr(C)]
        struct NativeWindowBuffer {
            width: i32,
            height: i32,
            stride: i32,
            format: i32,
            bits: *mut c_void,
            reserved: [u32; 6],
        }

        #[repr(C)]
        struct AImageReader {
            _private: [u8; 0],
        }

        #[repr(C)]
        struct AImage {
            _private: [u8; 0],
        }

        #[link(name = "android")]
        unsafe extern "C" {
            fn ANativeWindow_setBuffersGeometry(
                window: *mut ANativeWindow,
                width: i32,
                height: i32,
                format: i32,
            ) -> i32;
            fn ANativeWindow_lock(
                window: *mut ANativeWindow,
                buffer: *mut NativeWindowBuffer,
                dirty: *mut c_void,
            ) -> i32;
            fn ANativeWindow_unlockAndPost(window: *mut ANativeWindow) -> i32;
        }

        /// Binder threads for the test process. An app always has them; a bare executable does
        /// not, and without them the camera cannot dequeue buffers from surfaces whose queue
        /// lives in this process (it blocks, and closing the camera then times out).
        fn start_binder_threads() {
            static STARTED: std::sync::Once = std::sync::Once::new();
            // SAFETY: `libbinder_ndk.so` (Android 10+) is never closed, and
            // `ABinderProcess_startThreadPool` takes no arguments and returns nothing; it is
            // started once per process.
            STARTED.call_once(|| unsafe {
                let library = libc::dlopen(c"libbinder_ndk.so".as_ptr(), libc::RTLD_NOW);
                assert!(!library.is_null());
                let start = libc::dlsym(library, c"ABinderProcess_startThreadPool".as_ptr());
                assert!(!start.is_null());
                let start: unsafe extern "C" fn() = std::mem::transmute(start);
                start();
            });
        }

        #[link(name = "mediandk")]
        unsafe extern "C" {
            fn AImageReader_new(
                width: i32,
                height: i32,
                format: i32,
                max_images: i32,
                reader: *mut *mut AImageReader,
            ) -> i32;
            fn AImageReader_getWindow(
                reader: *mut AImageReader,
                window: *mut *mut ANativeWindow,
            ) -> i32;
            fn AImageReader_acquireLatestImage(
                reader: *mut AImageReader,
                image: *mut *mut AImage,
            ) -> i32;
            fn AImageReader_delete(reader: *mut AImageReader);
            fn AImage_getWidth(image: *const AImage, width: *mut i32) -> i32;
            fn AImage_getHeight(image: *const AImage, height: *mut i32) -> i32;
            fn AImage_getPlaneData(
                image: *const AImage,
                plane: i32,
                data: *mut *mut u8,
                length: *mut i32,
            ) -> i32;
            fn AImage_delete(image: *mut AImage);
        }

        const WIDTH: u32 = 640;
        const HEIGHT: u32 = 480;
        const WINDOW_FORMAT_RGBA_8888: i32 = 1;
        const IMAGE_FORMAT_YUV_420_888: i32 = 0x23;

        /// Draws one frame into the encoder's input surface from the CPU: the left half white,
        /// the right half black.
        fn draw_frame(window: &Window) {
            let mut buffer = NativeWindowBuffer {
                width: 0,
                height: 0,
                stride: 0,
                format: 0,
                bits: ptr::null_mut(),
                reserved: [0; 6],
            };
            // SAFETY: the window is live; the locked buffer is `stride * height` RGBA pixels
            // until `unlockAndPost`.
            unsafe {
                assert_eq!(
                    ANativeWindow_lock(window.as_ptr(), &mut buffer, ptr::null_mut()),
                    0
                );
                let (width, height, stride) = (
                    buffer.width as usize,
                    buffer.height as usize,
                    buffer.stride as usize,
                );
                let pixels =
                    std::slice::from_raw_parts_mut(buffer.bits.cast::<u32>(), stride * height);
                for row in pixels.chunks_mut(stride) {
                    for (x, pixel) in row.iter_mut().take(width).enumerate() {
                        *pixel = if x < width / 2 {
                            0xFFFF_FFFF
                        } else {
                            0xFF00_0000
                        };
                    }
                }
                assert_eq!(ANativeWindow_unlockAndPost(window.as_ptr()), 0);
            }
        }

        /// A YUV image reader the decoder renders into, standing in for the app's surface.
        struct Reader(*mut AImageReader);

        impl Reader {
            fn new() -> Self {
                let mut reader = ptr::null_mut();
                // SAFETY: `reader` is a valid place.
                let status = unsafe {
                    AImageReader_new(
                        WIDTH as i32,
                        HEIGHT as i32,
                        IMAGE_FORMAT_YUV_420_888,
                        4,
                        &mut reader,
                    )
                };
                assert_eq!(status, 0);
                Self(reader)
            }

            fn window(&self) -> NonNull<ANativeWindow> {
                let mut window = ptr::null_mut();
                // SAFETY: the reader is live; the window stays owned by it.
                assert_eq!(unsafe { AImageReader_getWindow(self.0, &mut window) }, 0);
                NonNull::new(window).unwrap()
            }

            /// The latest image's size and the share of bright and of dark luma samples, in
            /// percent. Shares, not places: hardware decoders may write a tiled layout (MediaTek
            /// does) that a CPU read of an `ImageReader` sees scrambled.
            fn latest(&self) -> Option<((i32, i32), usize, usize)> {
                let mut image = ptr::null_mut();
                // SAFETY: the reader is live; the image's planes are valid until deleted.
                unsafe {
                    if AImageReader_acquireLatestImage(self.0, &mut image) != 0 || image.is_null() {
                        return None;
                    }
                    let (mut width, mut height, mut length) = (0, 0, 0);
                    let mut data = ptr::null_mut();
                    AImage_getWidth(image, &mut width);
                    AImage_getHeight(image, &mut height);
                    AImage_getPlaneData(image, 0, &mut data, &mut length);
                    let luma = std::slice::from_raw_parts(data, length as usize);
                    let percent = |count: usize| count * 100 / luma.len().max(1);
                    let bright = percent(luma.iter().filter(|&&y| y > 200).count());
                    let dark = percent(luma.iter().filter(|&&y| y < 40).count());
                    AImage_delete(image);
                    Some(((width, height), bright, dark))
                }
            }
        }

        /// A reader moved to its consumer thread.
        struct SendReader(Reader);
        // SAFETY: an image reader may be used from any thread; one thread at a time does.
        unsafe impl Send for SendReader {}

        impl Drop for Reader {
            fn drop(&mut self) {
                // SAFETY: created in `new`, deleted once.
                unsafe { AImageReader_delete(self.0) };
            }
        }

        #[test]
        fn the_sources_and_sinks_can_move_to_another_thread() {
            fn assert_send<T: Send>() {}
            assert_send::<CameraSource>();
            assert_send::<DisplaySink>();
        }

        #[test]
        fn the_decoder_side_loads() {
            DisplaySink::new().start().unwrap();
        }

        // Encoder (input surface fed from the CPU instead of the camera) -> Annex-B access
        // units -> DisplaySink -> an ImageReader standing in for the app's surface.
        #[test]
        #[ignore]
        fn encodes_synthetic_frames_and_decodes_them_onto_a_surface() {
            start_binder_threads();
            let (sender, receiver) = frame_channel(128);
            let shared = Arc::new(EncoderShared::default());
            shared.set_rotation(Rotation::Deg90);
            let config = VideoConfig::default();
            let encoder = Encoder::start((WIDTH, HEIGHT), &config, sender, shared).unwrap();
            // SAFETY: the window is live (held by the encoder).
            unsafe {
                ANativeWindow_setBuffersGeometry(
                    encoder.input().as_ptr(),
                    WIDTH as i32,
                    HEIGHT as i32,
                    WINDOW_FORMAT_RGBA_8888,
                )
            };
            for n in 0..90 {
                draw_frame(encoder.input());
                if n == 30 {
                    encoder.set_bitrate(300_000);
                }
                if n == 45 {
                    encoder.request_keyframe();
                }
                sleep(Duration::from_millis(33));
            }
            sleep(Duration::from_millis(300));
            drop(encoder);
            let frames: Vec<EncodedFrame> =
                std::iter::from_fn(|| receiver.try_recv().ok()).collect();

            let keyframes: Vec<usize> = frames
                .iter()
                .enumerate()
                .filter(|(_, frame)| frame.keyframe)
                .map(|(n, _)| n)
                .collect();
            let bytes: usize = frames.iter().map(|frame| frame.data.len()).sum();
            eprintln!(
                "{} frames, {bytes} bytes, keyframes at {keyframes:?}, dropped {}",
                frames.len(),
                receiver.dropped()
            );
            assert!(frames.len() >= 60, "{} frames", frames.len());
            assert_eq!(keyframes.first(), Some(&0));
            assert!(
                keyframes.len() >= 2,
                "the requested keyframe: {keyframes:?}"
            );
            for &n in &keyframes {
                let params = StreamParams::from_keyframe(&frames[n]).unwrap();
                assert_eq!((params.width, params.height), (WIDTH, HEIGHT));
            }
            let sps = &nal_units(&frames[0].data)[0];
            eprintln!("SPS profile {:02x} {:02x} {:02x}", sps[1], sps[2], sps[3]);
            assert!(frames[0].data.starts_with(&[0, 0, 0, 1, 0x67]));
            assert!(frames.iter().all(|frame| frame.rotation == Rotation::Deg90));
            assert!(
                frames
                    .windows(2)
                    .all(|pair| pair[0].timestamp < pair[1].timestamp)
            );

            let reader = Reader::new();
            let mut sink = DisplaySink::new();
            // SAFETY: the reader's window is live until the reader goes, after the sink.
            unsafe { sink.set_surface(Some(reader.window())) };
            sink.start().unwrap();
            assert!(sink.keyframe_needed());
            let mut images = Vec::new();
            for (n, frame) in frames.iter().enumerate() {
                if n == 40 {
                    // The app's surface goes and comes back: no reset expected.
                    // SAFETY: as above.
                    unsafe { sink.set_surface(None) };
                }
                if n == 50 {
                    // SAFETY: as above.
                    unsafe { sink.set_surface(Some(reader.window())) };
                }
                sink.push(frame.clone()).unwrap();
                sleep(Duration::from_millis(15));
                images.extend(reader.latest());
                if n > 0 && n != 40 {
                    assert!(!sink.keyframe_needed(), "keyframe needed at {n}");
                }
            }
            sleep(Duration::from_millis(200));
            images.extend(reader.latest());
            eprintln!(
                "{} images; first {:?}, last {:?}; video size {:?}",
                images.len(),
                images.first(),
                images.last(),
                sink.video_size()
            );
            assert_eq!(sink.video_size(), Some((HEIGHT, WIDTH)));
            sink.stop().unwrap();
            drop(sink);

            assert!(images.len() >= 30, "{} images", images.len());
            // Half white, half black, as drawn.
            let ((width, height), bright, dark) = images[images.len() - 1];
            assert_eq!((width, height), (WIDTH as i32, HEIGHT as i32));
            assert!((45..=55).contains(&bright), "{bright} % bright");
            assert!((45..=55).contains(&dark), "{dark} % dark");
        }

        // A decoder error in the middle: garbage in place of a frame.
        #[test]
        #[ignore]
        fn a_sink_asks_for_a_keyframe_after_a_lost_frame() {
            let mut sink = DisplaySink::new();
            sink.start().unwrap();
            let keyframe = EncodedFrame {
                data: annex_b(&[&SPS_640X480, &PPS_X264, &IDR]),
                keyframe: true,
                timestamp: Duration::ZERO,
                rotation: Rotation::Deg0,
            };
            sink.push(keyframe).unwrap();
            assert!(!sink.keyframe_needed());
            // Without a surface the decoder renders nowhere; the request is the point here.
            sink.push(delta(Rotation::Deg90)).unwrap();
            assert!(sink.keyframe_needed(), "a turn asks for a keyframe");
            sink.stop().unwrap();
            assert!(!sink.keyframe_needed());
        }

        // Front camera with a preview, switch to the back one, drop the preview. From `adb
        // shell` the camera may be refused: that is the permission path.
        #[test]
        #[ignore]
        fn captures_from_both_cameras_or_reports_permission_denied() {
            start_binder_threads();
            let (sender, receiver) = frame_channel(256);
            let preview = Reader::new();
            let mut source = CameraSource::new();
            // SAFETY: the reader's window is live until the reader goes, after the source.
            unsafe { source.set_preview_surface(Some(preview.window())) }.unwrap();
            match source.start(VideoConfig::default(), Facing::Front, sender) {
                Err(VideoError::PermissionDenied) => {
                    eprintln!("camera: permission denied");
                    return;
                }
                Err(error) => panic!("camera: {error}"),
                Ok(()) => {}
            }
            eprintln!("capturing at {:?}", source.capture_size());
            // The preview's consumer keeps taking images on its own thread, as a SurfaceView
            // does: a camera whose output is full cannot drain, nor close in time.
            let previews = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let consumer = {
                let (previews, stop) = (previews.clone(), stop.clone());
                let preview = SendReader(preview);
                std::thread::spawn(move || {
                    let preview = preview;
                    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                        if preview.0.latest().is_some() {
                            previews.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        }
                        sleep(Duration::from_millis(5));
                    }
                    preview
                })
            };
            let count = || previews.load(std::sync::atomic::Ordering::Relaxed);
            sleep(Duration::from_secs(1));
            let front_previews = count();
            let front_frames: Vec<EncodedFrame> =
                std::iter::from_fn(|| receiver.try_recv().ok()).collect();
            source.switch_camera(Facing::Back).unwrap();
            sleep(Duration::from_secs(1));
            let back_previews = count() - front_previews;
            // SAFETY: `None` is always valid.
            unsafe { source.set_preview_surface(None) }.unwrap();
            sleep(Duration::from_millis(500));
            assert!(!source.camera_lost());
            source.stop().unwrap();
            assert!(front_frames.first().is_some_and(|frame| frame.keyframe));
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
            let _preview = consumer.join().unwrap();
            let back_frames: Vec<EncodedFrame> =
                std::iter::from_fn(|| receiver.try_recv().ok()).collect();
            for (name, frames, previews) in [
                ("front", &front_frames, front_previews),
                ("back", &back_frames, back_previews),
            ] {
                let keyframes = frames.iter().filter(|frame| frame.keyframe).count();
                let rotations: Vec<u16> = frames
                    .iter()
                    .map(|frame| frame.rotation.degrees())
                    .collect();
                let mut distinct = rotations.clone();
                distinct.dedup();
                eprintln!(
                    "{name}: {} frames, {keyframes} keyframes, rotations {distinct:?}, {previews} previews, size {:?}",
                    frames.len(),
                    frames
                        .iter()
                        .find_map(StreamParams::from_keyframe)
                        .map(|params| (params.width, params.height)),
                );
                assert!(frames.len() > 15, "{name}: {} frames", frames.len());
                // The front batch starts the stream; the back one gets the keyframe asked for
                // on the switch (after a few frames still in the encoder from the front).
                assert!(keyframes >= 1, "{name}: no keyframe");
                assert!(previews > 0, "{name}: no preview");
            }
        }
    }
}
