//! H.264 bitstream plumbing between VideoToolbox / CoreMedia (AVCC, length-prefixed NAL units)
//! and the engine (Annex-B, start codes). Pure functions, tested on the host.

/// The four-byte Annex-B start code this module writes.
pub(crate) const START_CODE: [u8; 4] = [0, 0, 0, 1];

/// NAL unit types (ITU-T H.264, table 7-1) this module cares about.
pub(crate) const NAL_SLICE: u8 = 1;
pub(crate) const NAL_IDR: u8 = 5;
pub(crate) const NAL_SEI: u8 = 6;
pub(crate) const NAL_SPS: u8 = 7;
pub(crate) const NAL_PPS: u8 = 8;
pub(crate) const NAL_AUD: u8 = 9;

/// Why a bitstream could not be converted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum H264Error {
    /// A NAL unit length prefix other than 1, 2 or 4 bytes.
    BadLengthSize,
    /// A length prefix runs past the end of the buffer.
    Truncated,
    /// No NAL unit at all, or an empty one.
    Empty,
}

/// The type of a NAL unit, from its header byte.
pub(crate) fn nal_type(nal: &[u8]) -> Option<u8> {
    nal.first().map(|header| header & 0x1f)
}

/// Splits AVCC data (each NAL unit behind a big-endian length of `length_size` bytes).
pub(crate) fn avcc_nals(data: &[u8], length_size: usize) -> Result<Vec<&[u8]>, H264Error> {
    if !matches!(length_size, 1 | 2 | 4) {
        return Err(H264Error::BadLengthSize);
    }
    let mut nals = Vec::new();
    let mut rest = data;
    while !rest.is_empty() {
        let (prefix, tail) = rest
            .split_at_checked(length_size)
            .ok_or(H264Error::Truncated)?;
        let len = prefix
            .iter()
            .fold(0usize, |len, &byte| (len << 8) | usize::from(byte));
        if len == 0 {
            return Err(H264Error::Empty);
        }
        let (nal, tail) = tail.split_at_checked(len).ok_or(H264Error::Truncated)?;
        nals.push(nal);
        rest = tail;
    }
    if nals.is_empty() {
        return Err(H264Error::Empty);
    }
    Ok(nals)
}

/// Splits Annex-B data on `00 00 01` and `00 00 00 01` start codes. Bytes before the first start
/// code, empty NAL units and the zero bytes that trail a NAL unit are left out.
pub(crate) fn annexb_nals(data: &[u8]) -> Vec<&[u8]> {
    let mut nals = Vec::new();
    let mut start = None;
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i..i + 3] == [0, 0, 1] {
            if let Some(from) = start {
                push_trimmed(&mut nals, &data[from..i]);
            }
            i += 3;
            start = Some(i);
        } else {
            i += 1;
        }
    }
    if let Some(from) = start {
        push_trimmed(&mut nals, &data[from..]);
    }
    nals
}

/// Pushes `nal` without its trailing zero bytes (the first byte of a four-byte start code, or
/// `trailing_zero_8bits`), unless nothing is left.
fn push_trimmed<'a>(nals: &mut Vec<&'a [u8]>, nal: &'a [u8]) {
    let end = nal
        .iter()
        .rposition(|&byte| byte != 0)
        .map_or(0, |last| last + 1);
    if end > 0 {
        nals.push(&nal[..end]);
    }
}

/// Whether the NAL units hold an IDR slice.
pub(crate) fn contains_idr(nals: &[&[u8]]) -> bool {
    nals.iter().any(|nal| nal_type(nal) == Some(NAL_IDR))
}

/// One Annex-B access unit from `nals`. When `parameter_sets` is not empty they go first and any
/// SPS or PPS already among `nals` is left out, so a keyframe carries exactly one copy.
pub(crate) fn write_annexb(nals: &[&[u8]], parameter_sets: &[&[u8]]) -> Vec<u8> {
    let skip_in_band = !parameter_sets.is_empty();
    let body = nals
        .iter()
        .filter(|nal| !(skip_in_band && matches!(nal_type(nal), Some(NAL_SPS | NAL_PPS))));
    let units: Vec<&[u8]> = parameter_sets
        .iter()
        .copied()
        .chain(body.copied())
        .collect();
    let size = units.iter().map(|nal| START_CODE.len() + nal.len()).sum();
    let mut out = Vec::with_capacity(size);
    for nal in units {
        out.extend_from_slice(&START_CODE);
        out.extend_from_slice(nal);
    }
    out
}

/// An Annex-B access unit taken apart for CoreMedia.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReceivedUnit<'a> {
    /// The first SPS in the unit, if any.
    pub sps: Option<&'a [u8]>,
    /// The first PPS in the unit, if any.
    pub pps: Option<&'a [u8]>,
    /// Whether it holds an IDR slice.
    pub keyframe: bool,
    /// The remaining NAL units (slices and SEI; no SPS, PPS or access unit delimiter) in AVCC
    /// with four-byte lengths, what a `CMSampleBuffer` with a `NALUnitHeaderLength` of 4 takes.
    pub avcc: Vec<u8>,
}

/// Takes an Annex-B access unit apart. It fails if the unit holds no slice.
pub(crate) fn parse_annexb(data: &[u8]) -> Result<ReceivedUnit<'_>, H264Error> {
    let mut unit = ReceivedUnit {
        sps: None,
        pps: None,
        keyframe: false,
        avcc: Vec::with_capacity(data.len()),
    };
    let mut slices = 0;
    for nal in annexb_nals(data) {
        match nal_type(nal) {
            Some(NAL_SPS) => {
                unit.sps.get_or_insert(nal);
            }
            Some(NAL_PPS) => {
                unit.pps.get_or_insert(nal);
            }
            Some(NAL_AUD) => {}
            kind => {
                if matches!(kind, Some(NAL_SLICE..=NAL_IDR)) {
                    slices += 1;
                }
                unit.keyframe |= kind == Some(NAL_IDR);
                let Ok(len) = u32::try_from(nal.len()) else {
                    return Err(H264Error::Truncated);
                };
                unit.avcc.extend_from_slice(&len.to_be_bytes());
                unit.avcc.extend_from_slice(nal);
            }
        }
    }
    if slices == 0 {
        return Err(H264Error::Empty);
    }
    Ok(unit)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPS: &[u8] = &[0x67, 0x42, 0xe0, 0x1f, 0x8c];
    const PPS: &[u8] = &[0x68, 0xce, 0x3c, 0x80];
    const IDR: &[u8] = &[0x65, 0x88, 0x84, 0x00, 0x21];
    const SLICE: &[u8] = &[0x41, 0x9a, 0x02];
    const SEI: &[u8] = &[0x06, 0x05, 0x01, 0x80];

    fn avcc(nals: &[&[u8]], length_size: usize) -> Vec<u8> {
        let mut out = Vec::new();
        for nal in nals {
            let len = (nal.len() as u32).to_be_bytes();
            out.extend_from_slice(&len[4 - length_size..]);
            out.extend_from_slice(nal);
        }
        out
    }

    #[test]
    fn the_nal_type_is_the_low_five_bits_of_the_header() {
        assert_eq!(nal_type(SPS), Some(NAL_SPS));
        assert_eq!(nal_type(PPS), Some(NAL_PPS));
        assert_eq!(nal_type(IDR), Some(NAL_IDR));
        assert_eq!(nal_type(SLICE), Some(NAL_SLICE));
        assert_eq!(nal_type(SEI), Some(NAL_SEI));
        assert_eq!(nal_type(&[]), None);
    }

    #[test]
    fn avcc_is_split_with_every_length_size() {
        for size in [1, 2, 4] {
            let data = avcc(&[SEI, IDR], size);
            assert_eq!(avcc_nals(&data, size), Ok(vec![SEI, IDR]), "{size}");
        }
    }

    #[test]
    fn bad_avcc_is_rejected() {
        assert_eq!(
            avcc_nals(&[0, 0, 0, 1, 0x65], 3),
            Err(H264Error::BadLengthSize)
        );
        assert_eq!(avcc_nals(&[0, 0, 0, 9, 0x65], 4), Err(H264Error::Truncated));
        assert_eq!(avcc_nals(&[0, 0], 4), Err(H264Error::Truncated));
        assert_eq!(avcc_nals(&[0, 0, 0, 0], 4), Err(H264Error::Empty));
        assert_eq!(avcc_nals(&[], 4), Err(H264Error::Empty));
    }

    #[test]
    fn annexb_is_split_on_both_start_codes() {
        let mut data = vec![0, 0, 0, 1];
        data.extend_from_slice(SPS);
        data.extend_from_slice(&[0, 0, 1]);
        data.extend_from_slice(PPS);
        data.extend_from_slice(&[0, 0, 0, 1]);
        data.extend_from_slice(IDR);
        assert_eq!(annexb_nals(&data), vec![SPS, PPS, IDR]);
    }

    #[test]
    fn annexb_leading_garbage_empty_units_and_trailing_zeros_are_left_out() {
        let mut data = vec![0xff, 0, 0, 1, 0, 0, 1];
        data.extend_from_slice(SLICE);
        data.extend_from_slice(&[0, 0, 0, 0, 0, 1]);
        data.extend_from_slice(SEI);
        data.extend_from_slice(&[0, 0]);
        assert_eq!(annexb_nals(&data), vec![SLICE, SEI]);
        assert!(annexb_nals(&[1, 2, 3]).is_empty());
    }

    #[test]
    fn an_idr_slice_makes_a_keyframe() {
        assert!(contains_idr(&[SEI, IDR]));
        assert!(!contains_idr(&[SEI, SLICE]));
        assert!(!contains_idr(&[]));
    }

    #[test]
    fn a_keyframe_gets_its_parameter_sets_first() {
        let data = write_annexb(&[SEI, IDR], &[SPS, PPS]);
        let mut expected = Vec::new();
        for nal in [SPS, PPS, SEI, IDR] {
            expected.extend_from_slice(&START_CODE);
            expected.extend_from_slice(nal);
        }
        assert_eq!(data, expected);
    }

    #[test]
    fn parameter_sets_already_in_band_are_not_repeated() {
        let data = write_annexb(&[SPS, PPS, IDR], &[SPS, PPS]);
        assert_eq!(annexb_nals(&data), vec![SPS, PPS, IDR]);
    }

    #[test]
    fn a_delta_frame_is_written_as_it_came() {
        let data = write_annexb(&[SLICE], &[]);
        assert_eq!(data, [&START_CODE[..], SLICE].concat());
    }

    #[test]
    fn a_received_keyframe_gives_its_parameter_sets_and_avcc_slices() {
        let data = write_annexb(&[SEI, IDR], &[SPS, PPS]);
        let unit = parse_annexb(&data).unwrap();
        assert_eq!(unit.sps, Some(SPS));
        assert_eq!(unit.pps, Some(PPS));
        assert!(unit.keyframe);
        assert_eq!(unit.avcc, avcc(&[SEI, IDR], 4));
    }

    #[test]
    fn a_received_delta_frame_has_no_parameter_sets() {
        let mut data = write_annexb(&[SLICE], &[]);
        // An access unit delimiter in front is dropped.
        data.splice(0..0, [0, 0, 0, 1, 0x09, 0xf0]);
        let unit = parse_annexb(&data).unwrap();
        assert_eq!(unit.sps, None);
        assert_eq!(unit.pps, None);
        assert!(!unit.keyframe);
        assert_eq!(unit.avcc, avcc(&[SLICE], 4));
    }

    #[test]
    fn a_unit_without_a_slice_is_rejected() {
        let data = write_annexb(&[SPS, PPS], &[]);
        assert_eq!(parse_annexb(&data), Err(H264Error::Empty));
        assert_eq!(parse_annexb(&[]), Err(H264Error::Empty));
    }

    #[test]
    fn avcc_to_annexb_and_back_is_lossless() {
        let encoded = avcc(&[SEI, IDR], 4);
        let nals = avcc_nals(&encoded, 4).unwrap();
        let annexb = write_annexb(&nals, &[SPS, PPS]);
        assert_eq!(parse_annexb(&annexb).unwrap().avcc, encoded);
    }
}
