//! Android video: the camera with its hardware encoder, and the hardware decoder with its
//! surface.

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
