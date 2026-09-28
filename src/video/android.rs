//! Android video: the camera with its hardware encoder, and the hardware decoder with its
//! surface.

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
}
