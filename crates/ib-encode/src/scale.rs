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
/// thin lines such as text and pen strokes don't vanish). Rows are split
/// across a few threads: this runs on every streamed frame when the screen
/// is larger than the tablet's picture (2560x1440 → 1920x1080 took 17 ms on
/// one core, most of the Linux encode time).
pub fn scale_bgra(src: &[u8], sw: u32, sh: u32, dw: u32, dh: u32) -> Vec<u8> {
    let (sw, sh, dw, dh) = (sw as usize, sh as usize, dw as usize, dh as usize);
    if sw == dw && sh == dh {
        return src[..sw * sh * 4].to_vec();
    }
    let xs = spans(sw, dw);
    let ys = spans(sh, dh);
    let mut out = vec![0u8; dw * dh * 4];
    let threads = std::thread::available_parallelism()
        .map_or(1, std::num::NonZero::get)
        .clamp(1, MAX_SCALE_THREADS)
        .min(dh.div_ceil(MIN_ROWS_PER_THREAD));
    let rows_per = dh.div_ceil(threads.max(1));
    std::thread::scope(|scope| {
        for (chunk, rows) in out.chunks_mut(rows_per * dw * 4).zip(ys.chunks(rows_per)) {
            let xs = &xs;
            scope.spawn(move || scale_rows(src, sw, xs, rows, chunk));
        }
    });
    out
}

/// Threads for [`scale_bgra`]: enough to bring a 1440p frame to a few ms.
const MAX_SCALE_THREADS: usize = 8;
/// Below this, a thread's start costs more than it saves.
const MIN_ROWS_PER_THREAD: usize = 64;

/// Source range `[start, end)` covered by each of `dst` output pixels
/// along an axis of `src` pixels (never empty).
fn spans(src: usize, dst: usize) -> Vec<(usize, usize)> {
    (0..dst)
        .map(|d| {
            let a = d * src / dst;
            let b = ((d + 1) * src / dst).max(a + 1).min(src);
            (a, b)
        })
        .collect()
}

/// Fills `out` (whole output rows) with the averages over `rows` × `xs`.
fn scale_rows(
    src: &[u8],
    sw: usize,
    xs: &[(usize, usize)],
    rows: &[(usize, usize)],
    out: &mut [u8],
) {
    let dw = xs.len();
    let mut acc = vec![0u32; dw * 4];
    for (r, &(y0, y1)) in rows.iter().enumerate() {
        acc.fill(0);
        for sy in y0..y1 {
            let line = &src[sy * sw * 4..(sy + 1) * sw * 4];
            for (sum, &(x0, x1)) in acc.as_chunks_mut::<4>().0.iter_mut().zip(xs) {
                for px in line[x0 * 4..x1 * 4].as_chunks::<4>().0 {
                    for (a, &v) in sum.iter_mut().zip(px) {
                        *a += u32::from(v);
                    }
                }
            }
        }
        let line = &mut out[r * dw * 4..(r + 1) * dw * 4];
        for ((dst, sum), &(x0, x1)) in line
            .as_chunks_mut::<4>()
            .0
            .iter_mut()
            .zip(acc.as_chunks::<4>().0)
            .zip(xs)
        {
            let n = ((y1 - y0) * (x1 - x0)) as u32;
            for (d, &a) in dst.iter_mut().zip(sum) {
                *d = (a / n) as u8;
            }
        }
    }
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

    /// The single-threaded original, kept as the reference.
    fn reference(src: &[u8], sw: u32, sh: u32, dw: u32, dh: u32) -> Vec<u8> {
        let (sw, sh, dw, dh) = (sw as usize, sh as usize, dw as usize, dh as usize);
        let mut out = vec![0u8; dw * dh * 4];
        for y in 0..dh {
            let y0 = y * sh / dh;
            let y1 = ((y + 1) * sh / dh).max(y0 + 1).min(sh);
            for x in 0..dw {
                let x0 = x * sw / dw;
                let x1 = ((x + 1) * sw / dw).max(x0 + 1).min(sw);
                let mut acc = [0u32; 4];
                for sy in y0..y1 {
                    for sx in x0..x1 {
                        for (c, a) in acc.iter_mut().enumerate() {
                            *a += u32::from(src[(sy * sw + sx) * 4 + c]);
                        }
                    }
                }
                let n = ((y1 - y0) * (x1 - x0)) as u32;
                for (c, a) in acc.iter().enumerate() {
                    out[(y * dw + x) * 4 + c] = (a / n) as u8;
                }
            }
        }
        out
    }

    #[test]
    fn matches_the_reference_on_awkward_sizes() {
        for &(sw, sh, dw, dh) in &[
            (2560, 1440, 1920, 1080),
            (1001, 777, 640, 333),
            (300, 200, 299, 199),
            (17, 9, 4, 2),
            (64, 700, 30, 129),
        ] {
            let src: Vec<u8> = (0..sw * sh * 4).map(|i| (i * 7 % 253) as u8).collect();
            assert_eq!(
                scale_bgra(&src, sw, sh, dw, dh),
                reference(&src, sw, sh, dw, dh),
                "{sw}x{sh} -> {dw}x{dh}"
            );
        }
    }

    #[test]
    fn box_filter_averages() {
        // 2x1 → 1x1: black and white average to grey.
        let src = [0, 0, 0, 255, 255, 255, 255, 255];
        assert_eq!(scale_bgra(&src, 2, 1, 1, 1), vec![127, 127, 127, 255]);
    }
}
