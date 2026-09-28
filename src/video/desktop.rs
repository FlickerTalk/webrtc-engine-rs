//! Desktop video, for trying calls on a computer (feature `desktop`, macOS): the camera through
//! `nokhwa` (AVFoundation) encoded with [`SoftwareEncoder`](super::openh264::SoftwareEncoder),
//! and a window through `minifb` fed by [`SoftwareDecoder`](super::openh264::SoftwareDecoder).

use super::VideoError;
use super::openh264::I420Frame;

/// Repacks a packed 4:2:2 picture in `yuvs` order (Y0 Cb Y1 Cr, video range: what AVFoundation
/// gives for YUYV) into I420, averaging the chroma of each pair of rows. `stride` is the bytes
/// in a row, padding included.
pub fn yuyv_to_i420(
    width: u32,
    height: u32,
    stride: usize,
    data: &[u8],
) -> Result<I420Frame, VideoError> {
    let (columns, rows) = (width as usize, height as usize);
    let row_bytes = columns * 2;
    let needed = rows
        .saturating_sub(1)
        .checked_mul(stride)
        .and_then(|bytes| bytes.checked_add(row_bytes));
    if stride < row_bytes || needed.is_none_or(|needed| data.len() < needed) || rows % 2 != 0 {
        return Err(VideoError::Unsupported);
    }
    let row = |index: usize| &data[index * stride..index * stride + row_bytes];
    let mut y = Vec::with_capacity(columns * rows);
    for index in 0..rows {
        y.extend(row(index).iter().step_by(2));
    }
    let chroma = (columns / 2) * (rows / 2);
    let (mut u, mut v) = (Vec::with_capacity(chroma), Vec::with_capacity(chroma));
    for index in (0..rows).step_by(2) {
        let (top, bottom) = (row(index), row(index + 1));
        for (a, b) in top.chunks_exact(4).zip(bottom.chunks_exact(4)) {
            let average = |i: usize| ((u16::from(a[i]) + u16::from(b[i]) + 1) / 2) as u8;
            u.push(average(1));
            v.push(average(3));
        }
    }
    I420Frame::from_planes(width, height, y, u, v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yuyv_is_repacked_into_i420() {
        // 4×2 with two bytes of padding at the end of each row.
        #[rustfmt::skip]
        let yuyv = [
            10, 100, 20, 200,  30, 110, 40, 210,  0, 0,
            50, 102, 60, 202,  70, 112, 80, 212,  0, 0,
        ];
        let frame = yuyv_to_i420(4, 2, 10, &yuyv).unwrap();
        assert_eq!(frame.y(), [10, 20, 30, 40, 50, 60, 70, 80]);
        assert_eq!(frame.u(), [101, 111]);
        assert_eq!(frame.v(), [201, 211]);
    }

    #[test]
    fn a_short_yuyv_buffer_is_rejected() {
        assert_eq!(
            yuyv_to_i420(4, 2, 8, &[0; 15]),
            Err(VideoError::Unsupported)
        );
        assert_eq!(
            yuyv_to_i420(4, 2, 6, &[0; 16]),
            Err(VideoError::Unsupported)
        );
    }
}
