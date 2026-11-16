//! CPU downscaling for the software path (the hardware path scales on the GPU).

/// Largest size within `max_w × max_h` that keeps the aspect ratio and has
/// even dimensions (required by 4:2:0 video). Never upscales.
pub fn fit_within(width: u32, height: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    if width == 0 || height == 0 {
        return (2, 2);
    }
    let scale = (f64::from(max_w) / f64::from(width))
        .min(f64::from(max_h) / f64::from(height))
        .min(1.0);
    let even = |v: f64| ((v as u32) & !1).max(2);
    (
        even(f64::from(width) * scale),
        even(f64::from(height) * scale),
    )
}

/// Box-filter downscale of a tightly packed BGRA image (area averaging, so
/// thin lines such as text and pen strokes don't vanish).
pub fn scale_bgra(src: &[u8], sw: u32, sh: u32, dw: u32, dh: u32) -> Vec<u8> {
    let (sw, sh, dw, dh) = (sw as usize, sh as usize, dw as usize, dh as usize);
    if sw == dw && sh == dh {
        return src[..sw * sh * 4].to_vec();
    }
    let mut out = vec![0u8; dw * dh * 4];
    for y in 0..dh {
        let y0 = y * sh / dh;
        let y1 = ((y + 1) * sh / dh).max(y0 + 1).min(sh);
        for x in 0..dw {
            let x0 = x * sw / dw;
            let x1 = ((x + 1) * sw / dw).max(x0 + 1).min(sw);
            let mut acc = [0u32; 4];
            for sy in y0..y1 {
                let row = &src[(sy * sw + x0) * 4..(sy * sw + x1) * 4];
                for px in row.as_chunks::<4>().0 {
                    for (a, &v) in acc.iter_mut().zip(px) {
                        *a += u32::from(v);
                    }
                }
            }
            let n = ((y1 - y0) * (x1 - x0)) as u32;
            let o = (y * dw + x) * 4;
            for (dst, a) in out[o..o + 4].iter_mut().zip(acc) {
                *dst = (a / n) as u8;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_keeps_aspect_even_and_never_upscales() {
        assert_eq!(fit_within(2560, 1440, 1920, 1200), (1920, 1080));
        assert_eq!(fit_within(900, 1440, 1920, 1200), (750, 1200));
        assert_eq!(fit_within(1280, 720, 1920, 1200), (1280, 720));
        assert_eq!(fit_within(1001, 501, 4000, 4000), (1000, 500));
    }

    #[test]
    fn box_filter_averages() {
        // 2x1 → 1x1: black and white average to grey.
        let src = [0, 0, 0, 255, 255, 255, 255, 255];
        assert_eq!(scale_bgra(&src, 2, 1, 1, 1), vec![127, 127, 127, 255]);
    }
}
