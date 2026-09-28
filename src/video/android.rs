//! Android video: the camera with its hardware encoder, and the hardware decoder with its
//! surface.

use super::{EncodedFrame, Rotation};

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
}
