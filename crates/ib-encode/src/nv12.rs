//! BGRA → NV12 conversion (BT.709, limited range), for hardware encoders
//! fed from CPU frames. Matches the colour handling of the Media Foundation
//! path, so the tablet shows the same colours whichever encoder runs.

/// Destination planes with their row pitches (encoders pad rows).
#[derive(Debug)]
pub struct Nv12Planes<'a> {
    pub y: &'a mut [u8],
    pub y_pitch: usize,
    /// Interleaved Cb/Cr, half height.
    pub uv: &'a mut [u8],
    pub uv_pitch: usize,
}

// BT.709 limited range, 8.8 fixed point (Y = 16 + …, Cb/Cr = 128 + …).
const YR: i32 = 47;
const YG: i32 = 157;
const YB: i32 = 16;
const UR: i32 = -26;
const UG: i32 = -87;
const UB: i32 = 112;
const VR: i32 = 112;
const VG: i32 = -102;
const VB: i32 = -10;

#[inline]
fn luma(b: i32, g: i32, r: i32) -> u8 {
    (((YR * r + YG * g + YB * b + 128) >> 8) + 16) as u8
}

/// Converts a tightly packed `width`×`height` BGRA image. `width` and
/// `height` must be even; the planes must be large enough for their pitch.
pub fn bgra_to_nv12(bgra: &[u8], width: usize, height: usize, out: &mut Nv12Planes<'_>) {
    let stride = width * 4;
    for row in 0..height {
        let src = &bgra[row * stride..(row + 1) * stride];
        let dst = &mut out.y[row * out.y_pitch..row * out.y_pitch + width];
        for (d, px) in dst.iter_mut().zip(src.as_chunks::<4>().0) {
            *d = luma(i32::from(px[0]), i32::from(px[1]), i32::from(px[2]));
        }
    }
    for row in 0..height / 2 {
        let top = &bgra[2 * row * stride..(2 * row + 1) * stride];
        let bottom = &bgra[(2 * row + 1) * stride..(2 * row + 2) * stride];
        let dst = &mut out.uv[row * out.uv_pitch..row * out.uv_pitch + width];
        for (col, d) in dst.as_chunks_mut::<2>().0.iter_mut().enumerate() {
            // Average the 2x2 block.
            let i = col * 8;
            let sum = |c: usize| {
                i32::from(top[i + c])
                    + i32::from(top[i + 4 + c])
                    + i32::from(bottom[i + c])
                    + i32::from(bottom[i + 4 + c])
            };
            let (b, g, r) = (sum(0), sum(1), sum(2));
            d[0] = (((UR * r + UG * g + UB * b + 512) >> 10) + 128) as u8;
            d[1] = (((VR * r + VG * g + VB * b + 512) >> 10) + 128) as u8;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn convert(px: [u8; 4]) -> (u8, u8, u8) {
        let bgra = px.repeat(4);
        let (mut y, mut uv) = (vec![0u8; 4], vec![0u8; 2]);
        bgra_to_nv12(
            &bgra,
            2,
            2,
            &mut Nv12Planes {
                y: &mut y,
                y_pitch: 2,
                uv: &mut uv,
                uv_pitch: 2,
            },
        );
        assert!(y.iter().all(|&v| v == y[0]));
        (y[0], uv[0], uv[1])
    }

    #[test]
    fn reference_colours_land_in_limited_range() {
        // BT.709 limited: black (16,128,128), white (235,128,128),
        // red ≈ (63,102,240), blue ≈ (32,240,118).
        let close = |a: u8, b: u8| a.abs_diff(b) <= 1;
        let (y, u, v) = convert([0, 0, 0, 255]);
        assert_eq!((y, u, v), (16, 128, 128));
        let (y, u, v) = convert([255, 255, 255, 255]);
        assert!(
            close(y, 235) && close(u, 128) && close(v, 128),
            "{y} {u} {v}"
        );
        let (y, u, v) = convert([0, 0, 255, 255]);
        assert!(
            close(y, 63) && close(u, 102) && close(v, 240),
            "{y} {u} {v}"
        );
        let (y, u, v) = convert([255, 0, 0, 255]);
        assert!(
            close(y, 32) && close(u, 240) && close(v, 118),
            "{y} {u} {v}"
        );
    }

    #[test]
    fn respects_row_pitch() {
        let bgra = [255u8, 255, 255, 255].repeat(4 * 2);
        let (mut y, mut uv) = (vec![0u8; 8 * 2], vec![0u8; 8]);
        bgra_to_nv12(
            &bgra,
            4,
            2,
            &mut Nv12Planes {
                y: &mut y,
                y_pitch: 8,
                uv: &mut uv,
                uv_pitch: 8,
            },
        );
        assert_eq!(&y[4..8], &[0, 0, 0, 0]);
        assert!(y[8..12].iter().all(|&v| v >= 234));
        assert_eq!(&uv[4..], &[0, 0, 0, 0]);
    }
}
