//! Just enough of H.264 to move access units: splitting Annex-B into NAL units and telling
//! keyframes apart. Nothing here decodes a picture.

/// NAL unit type of a slice of a non-IDR picture: a delta frame.
pub const NAL_SLICE: u8 = 1;
/// NAL unit type of a slice of an IDR picture: a keyframe.
pub const NAL_IDR: u8 = 5;
/// NAL unit type of supplemental enhancement information.
pub const NAL_SEI: u8 = 6;
/// NAL unit type of a sequence parameter set.
pub const NAL_SPS: u8 = 7;
/// NAL unit type of a picture parameter set.
pub const NAL_PPS: u8 = 8;
/// NAL unit type of an access unit delimiter.
pub const NAL_AUD: u8 = 9;

/// The type of a NAL unit, from its header byte.
pub fn nal_type(header: u8) -> u8 {
    header & 0x1F
}

/// The NAL units of an Annex-B access unit, without their start codes. Either start code
/// (`00 00 01` or `00 00 00 01`) is accepted; bytes before the first start code are ignored.
pub fn nal_units(data: &[u8]) -> Vec<&[u8]> {
    let mut units = Vec::new();
    let mut start = None;
    let mut index = 0;
    while index + 3 <= data.len() {
        if data[index] == 0 && data[index + 1] == 0 && data[index + 2] == 1 {
            if let Some(begin) = start {
                units.push(trim_trailing_zeros(&data[begin..index]));
            }
            index += 3;
            start = Some(index);
        } else {
            index += 1;
        }
    }
    if let Some(begin) = start {
        units.push(&data[begin..]);
    }
    units.retain(|unit| !unit.is_empty());
    units
}

fn trim_trailing_zeros(unit: &[u8]) -> &[u8] {
    let end = unit
        .iter()
        .rposition(|&byte| byte != 0)
        .map_or(0, |last| last + 1);
    &unit[..end]
}

/// Whether an Annex-B access unit holds an IDR slice.
pub fn is_keyframe(data: &[u8]) -> bool {
    nal_units(data)
        .iter()
        .any(|unit| nal_type(unit[0]) == NAL_IDR)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPS: &[u8] = &[0x67, 0x42, 0xE0, 0x1F, 0x8C, 0x8D];
    const PPS: &[u8] = &[0x68, 0xCE, 0x3C, 0x80];
    const IDR: &[u8] = &[0x65, 0x88, 0x84, 0x21];
    const SLICE: &[u8] = &[0x41, 0x9A, 0x02, 0x03];

    fn annex_b(units: &[&[u8]]) -> Vec<u8> {
        units
            .iter()
            .flat_map(|unit| [&[0, 0, 0, 1][..], unit].concat())
            .collect()
    }

    #[test]
    fn splits_an_access_unit_into_its_nal_units() {
        let data = annex_b(&[SPS, PPS, IDR]);
        assert_eq!(nal_units(&data), vec![SPS, PPS, IDR]);
    }

    #[test]
    fn takes_three_byte_start_codes_too() {
        let mut data = vec![0, 0, 1];
        data.extend_from_slice(SPS);
        data.extend_from_slice(&[0, 0, 0, 1]);
        data.extend_from_slice(PPS);
        data.extend_from_slice(&[0, 0, 1]);
        data.extend_from_slice(SLICE);
        assert_eq!(nal_units(&data), vec![SPS, PPS, SLICE]);
    }

    #[test]
    fn keeps_zero_bytes_inside_a_nal_unit() {
        // A NAL unit never ends in a zero byte (its RBSP trailing bits), so zeros before a
        // start code belong to the start code.
        let unit: &[u8] = &[0x41, 0x00, 0x00, 0x03, 0x01, 0x80];
        let data = annex_b(&[unit, IDR]);
        assert_eq!(nal_units(&data), vec![unit, IDR]);
    }

    #[test]
    fn has_no_nal_units_without_a_start_code() {
        assert!(nal_units(&[]).is_empty());
        assert!(nal_units(&[0x65, 0x88]).is_empty());
    }

    #[test]
    fn an_idr_slice_makes_a_keyframe() {
        assert!(is_keyframe(&annex_b(&[SPS, PPS, IDR])));
        assert!(is_keyframe(&annex_b(&[IDR])));
        assert!(!is_keyframe(&annex_b(&[SLICE])));
        // Parameter sets alone decode no picture.
        assert!(!is_keyframe(&annex_b(&[SPS, PPS])));
        assert!(!is_keyframe(&[]));
    }

    #[test]
    fn reads_the_type_from_the_header_byte() {
        assert_eq!(nal_type(0x67), NAL_SPS);
        assert_eq!(nal_type(0x68), NAL_PPS);
        assert_eq!(nal_type(0x65), NAL_IDR);
        assert_eq!(nal_type(0x41), NAL_SLICE);
        assert_eq!(nal_type(0x06), NAL_SEI);
        assert_eq!(nal_type(0x09), NAL_AUD);
    }
}
