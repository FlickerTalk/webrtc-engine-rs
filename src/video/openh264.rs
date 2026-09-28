//! Software H.264 with Cisco's [OpenH264](https://github.com/cisco/openh264) (BSD-2-Clause),
//! compiled from source by the `openh264` crate with `cc`: no cmake, and no nasm on arm64.
//!
//! For tests (real encoded frames) and the desktop; phones use their hardware codecs.

use super::VideoError;

/// A picture in I420 (planar YUV 4:2:0, BT.601 limited range) in CPU memory: the format the
/// software encoder takes and the decoder gives back.
///
/// Width and height are even and non-zero; the chroma planes are half of each.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct I420Frame {
    width: u32,
    height: u32,
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
}

impl I420Frame {
    /// A frame from its three planes, packed without padding.
    pub fn from_planes(
        width: u32,
        height: u32,
        y: Vec<u8>,
        u: Vec<u8>,
        v: Vec<u8>,
    ) -> Result<Self, VideoError> {
        let (luma, chroma) = plane_sizes(width, height)?;
        if y.len() != luma || u.len() != chroma || v.len() != chroma {
            return Err(VideoError::Unsupported);
        }
        Ok(Self {
            width,
            height,
            y,
            u,
            v,
        })
    }

    /// Converts packed 8-bit RGB (3 bytes a pixel, rows without padding).
    pub fn from_rgb(width: u32, height: u32, rgb: &[u8]) -> Result<Self, VideoError> {
        Self::from_packed(width, height, rgb, 3)
    }

    /// Converts packed 8-bit RGBA; the alpha is ignored.
    pub fn from_rgba(width: u32, height: u32, rgba: &[u8]) -> Result<Self, VideoError> {
        Self::from_packed(width, height, rgba, 4)
    }

    fn from_packed(
        width: u32,
        height: u32,
        pixels: &[u8],
        bytes_per_pixel: usize,
    ) -> Result<Self, VideoError> {
        let (luma, chroma) = plane_sizes(width, height)?;
        if pixels.len() != luma * bytes_per_pixel {
            return Err(VideoError::Unsupported);
        }
        let (width_px, height_px) = (width as usize, height as usize);
        let rgb_at = |x: usize, y: usize| {
            let at = (y * width_px + x) * bytes_per_pixel;
            [
                i32::from(pixels[at]),
                i32::from(pixels[at + 1]),
                i32::from(pixels[at + 2]),
            ]
        };
        let mut y_plane = Vec::with_capacity(luma);
        for row in 0..height_px {
            for column in 0..width_px {
                let [r, g, b] = rgb_at(column, row);
                y_plane.push(clamp_u8(((66 * r + 129 * g + 25 * b + 128) >> 8) + 16));
            }
        }
        let (mut u_plane, mut v_plane) = (Vec::with_capacity(chroma), Vec::with_capacity(chroma));
        for row in (0..height_px).step_by(2) {
            for column in (0..width_px).step_by(2) {
                // The chroma of each 2×2 block is that of its average colour.
                let mut sum = [0; 3];
                for (x, y) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                    let pixel = rgb_at(column + x, row + y);
                    for channel in 0..3 {
                        sum[channel] += pixel[channel];
                    }
                }
                let [r, g, b] = sum.map(|total| (total + 2) / 4);
                u_plane.push(clamp_u8(((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128));
                v_plane.push(clamp_u8(((112 * r - 94 * g - 18 * b + 128) >> 8) + 128));
            }
        }
        Self::from_planes(width, height, y_plane, u_plane, v_plane)
    }

    /// The picture as packed 8-bit RGB.
    pub fn to_rgb(&self) -> Vec<u8> {
        self.to_packed(3)
    }

    /// The picture as packed 8-bit RGBA, opaque.
    pub fn to_rgba(&self) -> Vec<u8> {
        self.to_packed(4)
    }

    fn to_packed(&self, bytes_per_pixel: usize) -> Vec<u8> {
        let (width, height) = (self.width as usize, self.height as usize);
        let mut out = Vec::with_capacity(width * height * bytes_per_pixel);
        for row in 0..height {
            for column in 0..width {
                let chroma_at = (row / 2) * (width / 2) + column / 2;
                let c = 298 * (i32::from(self.y[row * width + column]) - 16);
                let d = i32::from(self.u[chroma_at]) - 128;
                let e = i32::from(self.v[chroma_at]) - 128;
                out.push(clamp_u8((c + 409 * e + 128) >> 8));
                out.push(clamp_u8((c - 100 * d - 208 * e + 128) >> 8));
                out.push(clamp_u8((c + 516 * d + 128) >> 8));
                if bytes_per_pixel == 4 {
                    out.push(u8::MAX);
                }
            }
        }
        out
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    /// Luma, `width × height` bytes.
    pub fn y(&self) -> &[u8] {
        &self.y
    }

    /// Blue-difference chroma, `width/2 × height/2` bytes.
    pub fn u(&self) -> &[u8] {
        &self.u
    }

    /// Red-difference chroma, `width/2 × height/2` bytes.
    pub fn v(&self) -> &[u8] {
        &self.v
    }
}

/// Bytes in the luma plane and in each chroma plane, for even, non-zero sizes.
fn plane_sizes(width: u32, height: u32) -> Result<(usize, usize), VideoError> {
    if width == 0 || height == 0 || width % 2 != 0 || height % 2 != 0 {
        return Err(VideoError::Unsupported);
    }
    let (width, height) = (width as usize, height as usize);
    Ok((width * height, (width / 2) * (height / 2)))
}

fn clamp_u8(value: i32) -> u8 {
    value.clamp(0, 255) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat_rgb(width: u32, height: u32, colour: [u8; 3]) -> Vec<u8> {
        colour.repeat((width * height) as usize)
    }

    #[test]
    fn rgb_colours_survive_the_round_trip_through_i420() {
        let colours = [
            [0, 0, 0],
            [255, 255, 255],
            [128, 128, 128],
            [255, 0, 0],
            [0, 255, 0],
            [0, 0, 255],
            [200, 120, 40],
        ];
        for colour in colours {
            let frame = I420Frame::from_rgb(4, 2, &flat_rgb(4, 2, colour)).unwrap();
            assert_eq!((frame.width(), frame.height()), (4, 2));
            assert_eq!(
                (frame.y().len(), frame.u().len(), frame.v().len()),
                (8, 2, 2)
            );
            let back = frame.to_rgb();
            assert_eq!(back.len(), 4 * 2 * 3);
            for pixel in back.chunks(3) {
                for (got, want) in pixel.iter().zip(colour) {
                    assert!(got.abs_diff(want) <= 3, "{colour:?} came back as {pixel:?}");
                }
            }
        }
    }

    #[test]
    fn rgba_ignores_the_alpha_and_comes_back_opaque() {
        let rgba = [200, 120, 40, 7].repeat(4);
        let frame = I420Frame::from_rgba(2, 2, &rgba).unwrap();
        assert_eq!(
            frame,
            I420Frame::from_rgb(2, 2, &[200, 120, 40].repeat(4)).unwrap()
        );
        let back = frame.to_rgba();
        assert_eq!(back.len(), 16);
        assert!(back.chunks(4).all(|pixel| pixel[3] == 255));
    }

    #[test]
    fn odd_or_empty_sizes_and_short_buffers_are_rejected() {
        assert_eq!(
            I420Frame::from_rgb(3, 2, &[0; 18]),
            Err(VideoError::Unsupported)
        );
        assert_eq!(I420Frame::from_rgb(0, 0, &[]), Err(VideoError::Unsupported));
        assert_eq!(
            I420Frame::from_rgb(2, 2, &[0; 11]),
            Err(VideoError::Unsupported)
        );
        assert_eq!(
            I420Frame::from_planes(2, 2, vec![0; 4], vec![0; 1], vec![0; 2]),
            Err(VideoError::Unsupported)
        );
        assert!(I420Frame::from_planes(2, 2, vec![0; 4], vec![0; 1], vec![0; 1]).is_ok());
    }
}
