//! `inkbridge-host --self-test [REPORT]` (Linux): checks the parts of the
//! Linux host that can't be tested in CI — the GPU encoder, the desktop's
//! screen-sharing and remote-control permission, PipeWire frames and the
//! pointer — and writes a plain-text report for the developers.
//!
//! Interactive: the desktop shows its permission dialog, and the user
//! confirms what they saw (the pointer moving). Nothing is clicked or typed.

use std::fs::File;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ib_capture::{Frame, FrameData};

/// Report lines go to the terminal and, flushed as they come, to the file
/// (so a crash still leaves everything up to that point).
struct Report {
    file: File,
    failures: Vec<String>,
}

impl Report {
    fn line(&mut self, text: &str) {
        println!("{text}");
        let _ = writeln!(self.file, "{text}");
        let _ = self.file.flush();
    }

    fn section(&mut self, title: &str) {
        self.line(&format!("\n== {title}"));
    }

    fn pass(&mut self, what: &str) {
        self.line(&format!("PASS  {what}"));
    }

    fn fail(&mut self, what: &str) {
        self.line(&format!("FAIL  {what}"));
        self.failures.push(what.to_owned());
    }

    fn info(&mut self, what: &str) {
        self.line(&format!("      {what}"));
    }
}

/// Runs every check; returns the process exit code (0 = all passed).
pub fn run(path: &Path) -> anyhow::Result<i32> {
    let file = File::create(path)?;
    let mut r = Report {
        file,
        failures: Vec::new(),
    };
    r.line(&format!(
        "Inkbridge Linux self-test, inkbridge-host {}",
        env!("CARGO_PKG_VERSION")
    ));
    system(&mut r);
    encoder(&mut r);
    wayland(&mut r, path.parent().unwrap_or(Path::new(".")));
    uinput(&mut r);

    r.section("Summary");
    if r.failures.is_empty() {
        r.line("All checks passed.");
    } else {
        let list = r.failures.join("\n  - ");
        r.line(&format!(
            "{} check(s) failed:\n  - {list}",
            r.failures.len()
        ));
    }
    r.line(&format!("Report saved to {}", path.display()));
    Ok(i32::from(!r.failures.is_empty()))
}

fn read(path: impl AsRef<Path>) -> String {
    std::fs::read_to_string(path).map_or_default(|s| s.trim().to_owned())
}

fn system(r: &mut Report) {
    r.section("System");
    let os = read("/etc/os-release")
        .lines()
        .find_map(|l| l.strip_prefix("PRETTY_NAME="))
        .unwrap_or("?")
        .trim_matches('"')
        .to_owned();
    r.info(&format!(
        "OS: {os}, kernel {}",
        read("/proc/sys/kernel/osrelease")
    ));
    let var = |k: &str| std::env::var(k).unwrap_or_else(|_| "-".into());
    r.info(&format!(
        "Session: {} ({}), WAYLAND_DISPLAY={}",
        var("XDG_SESSION_TYPE"),
        var("XDG_CURRENT_DESKTOP"),
        var("WAYLAND_DISPLAY")
    ));
    let mut nodes: Vec<PathBuf> = std::fs::read_dir("/sys/class/drm")
        .map_or_default(|d| d.flatten().map(|e| e.path()).collect());
    nodes.sort();
    for node in nodes {
        let name = node.file_name().unwrap_or_default().to_string_lossy();
        if !name.starts_with("renderD") {
            continue;
        }
        let dev = node.join("device");
        let driver = std::fs::read_link(dev.join("driver"))
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "?".into());
        r.info(&format!(
            "GPU /dev/dri/{name}: driver {driver}, PCI {}:{}",
            read(dev.join("vendor")),
            read(dev.join("device"))
        ));
    }
}

/// A synthetic test picture: a gradient that moves every frame.
fn test_frame(width: u32, height: u32, n: u32) -> Vec<u8> {
    let mut px = Vec::with_capacity(width as usize * height as usize * 4);
    for y in 0..height {
        for x in 0..width {
            let v = ((x + y + n * 8) % 256) as u8;
            px.extend_from_slice(&[v, 255 - v, 128, 255]);
        }
    }
    px
}

/// BT.709 limited-range luma, as the encoder computes it.
fn luma(bgra: &[u8]) -> u32 {
    ((47 * u32::from(bgra[2]) + 157 * u32::from(bgra[1]) + 16 * u32::from(bgra[0]) + 128) >> 8) + 16
}

/// Encodes `frames` and checks that an independent decoder reads them back
/// with the right picture size and brightness. Returns encode times.
fn encode_and_check(
    enc: &mut ib_encode::VaapiEncoder,
    frames: &[Frame],
) -> Result<(Vec<Duration>, usize), String> {
    use ib_encode::VideoEncoder;
    use openh264::formats::YUVSource;

    let mut decoder = openh264::decoder::Decoder::new().map_err(|e| e.to_string())?;
    let (mut times, mut bytes) = (Vec::new(), 0);
    for (i, f) in frames.iter().enumerate() {
        let started = Instant::now();
        let out = enc
            .encode(f, i == 0)
            .map_err(|e| format!("frame {i}: {e}"))?;
        times.push(started.elapsed());
        bytes += out.data.len();
        let pic = decoder
            .decode(&out.data)
            .map_err(|e| format!("frame {i} doesn't decode: {e}"))?
            .ok_or_else(|| format!("frame {i}: decoder returned no picture"))?;
        let (w, h) = pic.dimensions();
        if (w as u32, h as u32) != (f.width, f.height) {
            return Err(format!(
                "frame {i}: decoded {w}x{h}, expected {}x{}",
                f.width, f.height
            ));
        }
        let FrameData::Cpu(src) = &f.data else {
            continue;
        };
        // Compare a few points: catches swapped channels and wrong strides.
        for (fx, fy) in [(1, 1), (1, 3), (3, 1), (3, 3)] {
            let (x, y) = (w * fx / 4, h * fy / 4);
            let expected = luma(&src[(y * w + x) * 4..]);
            let got = u32::from(pic.y()[y * pic.strides().0 + x]);
            if got.abs_diff(expected) > 12 {
                return Err(format!(
                    "frame {i}: brightness at ({x},{y}) is {got}, expected about {expected}"
                ));
            }
        }
    }
    Ok((times, bytes))
}

fn timing(times: &mut [Duration]) -> String {
    times.sort();
    let ms = |d: Duration| d.as_secs_f64() * 1000.0;
    format!(
        "median {:.2} ms, p95 {:.2} ms, max {:.2} ms",
        ms(times[times.len() / 2]),
        ms(times[times.len() * 95 / 100]),
        ms(times[times.len() - 1])
    )
}

fn encoder(r: &mut Report) {
    use ib_encode::EncoderConfig;
    use ib_protocol::VideoCodec;

    r.section("Hardware video encoder (VA-API)");
    let (width, height) = (1920, 1080);
    let config = EncoderConfig {
        codec: VideoCodec::H264,
        width,
        height,
        fps: 60,
        bitrate_kbps: 20_000,
    };
    let mut enc = match ib_encode::VaapiEncoder::new(config) {
        Ok(e) => e,
        Err(e) => {
            r.fail(&format!("no VA-API H.264 encoder: {e}"));
            return;
        }
    };
    r.info(&format!("Device: {}", enc.device()));
    let frames: Vec<Frame> = (0..60)
        .map(|n| Frame {
            capture_t_us: u64::from(n),
            width,
            height,
            data: FrameData::Cpu(test_frame(width, height, n)),
        })
        .collect();
    match encode_and_check(&mut enc, &frames) {
        Ok((mut times, bytes)) => {
            r.pass("60 frames of 1920x1080 encode and decode correctly");
            r.info(&format!(
                "Encode time: {}; average {} KB/frame",
                timing(&mut times),
                bytes / 60 / 1024
            ));
        }
        Err(e) => r.fail(&format!("VA-API encoding: {e}")),
    }
}

fn wayland(r: &mut Report, results: &Path) {
    r.section("Wayland screen sharing (portal + PipeWire)");
    if !crate::wayland::is_wayland_session() {
        r.info("Not a Wayland session: skipped.");
        return;
    }
    println!(
        "\n>>> Your desktop will now ask to share the screen and allow remote control.\n\
         >>> Choose the monitor and allow it (if offered, also tick \"Remember this selection\")."
    );
    let started = Instant::now();
    let desktop = match crate::wayland::connect(&crate::storage::config_dir(), true) {
        Ok(d) => d,
        Err(e) => {
            r.fail(&format!("permission or portal: {e:#}"));
            return;
        }
    };
    r.pass(&format!(
        "screen sharing granted ({:.1} s including the dialog)",
        started.elapsed().as_secs_f64()
    ));

    let mut capture = desktop.capture();
    let display = match capture.displays() {
        Ok(d) if !d.is_empty() => d[0].bounds,
        other => {
            r.fail(&format!("no stream size from PipeWire: {other:?}"));
            return;
        }
    };
    r.info(&format!(
        "Stream: {}x{} pixels, logical scale {:.3}",
        display.width,
        display.height,
        desktop.logical_scale()
    ));
    if let Err(e) = capture.start(display) {
        r.fail(&format!("starting capture: {e}"));
        return;
    }

    let Some(frames) = frame_rate(r, desktop, capture.as_mut(), display) else {
        return;
    };
    let last = frames.last().expect("not empty");
    match save_ppm(last, &results.join("capture.ppm")) {
        Ok(p) => r.info(&format!("Last captured frame saved to {}", p.display())),
        Err(e) => r.info(&format!("could not save the captured frame: {e}")),
    }

    encode_captured(r, &frames);
    pointer(r, desktop, display);
}

/// Counts the frames that arrive in 5 s while the screen changes; returns a few for the encoder
/// check, or `None` after reporting a failure.
fn frame_rate(
    r: &mut Report,
    desktop: &crate::wayland::WaylandDesktop,
    capture: &mut dyn ib_capture::ScreenSource,
    display: ib_geometry::Region,
) -> Option<Vec<Frame>> {
    // The cursor is embedded in the stream, so moving it changes the screen on every step:
    // driving it ourselves measures the capture path, not how busily someone moves the mouse.
    let mut mover = desktop
        .pointer_sink()
        .map(|sink| CircleMover::new(sink, display));
    if mover.is_some() {
        println!(
            "\n>>> For the next 5 seconds the pointer moves in a circle by itself; leave the mouse alone."
        );
    } else {
        println!("\n>>> For the next 5 seconds, move the mouse or drag a window around.");
    }
    let mut frames = Vec::new();
    let mut arrivals = Vec::new();
    let window = Instant::now();
    while window.elapsed() < Duration::from_secs(5) {
        let wait = if let Some(m) = mover.as_mut() {
            if let Err(e) = m.step() {
                r.info(&format!("moving the pointer for the frame-rate check: {e}"));
                mover = None;
            }
            CircleMover::<dyn ib_inject::PenSink>::STEP
        } else {
            Duration::from_millis(100)
        };
        match capture.next_frame(wait) {
            Ok(Some(f)) => {
                arrivals.push(window.elapsed());
                // Keep a few for the encoder check; the rest only count.
                if frames.len() < 30 {
                    frames.push(f);
                }
            }
            Ok(None) => {}
            Err(e) => {
                r.fail(&format!("capture: {e}"));
                return None;
            }
        }
    }
    if frames.is_empty() {
        r.fail("no frames arrived in 5 s (did anything on screen change?)");
        return None;
    }
    let mut gaps: Vec<Duration> = arrivals
        .windows(2)
        .map(|w| w[1].saturating_sub(w[0]))
        .collect();
    r.pass(&format!(
        "{} frames in 5 s ({:.0} fps while the screen changed{})",
        arrivals.len(),
        arrivals.len() as f64 / 5.0,
        if mover.is_some() {
            ", pointer moved by the test"
        } else {
            ""
        }
    ));
    drop(mover);
    if gaps.len() > 2 {
        r.info(&format!("Gap between frames: {}", timing(&mut gaps)));
    }
    Some(frames)
}

/// Moves the pointer around a circle in the middle of `display`, by time, so each step lands on
/// a new spot and the screen keeps changing faster than any monitor refreshes.
struct CircleMover<S: ib_inject::PenSink + ?Sized> {
    sink: Box<S>,
    centre: (f64, f64),
    radius: f64,
    started: Instant,
    last: Option<Instant>,
}

impl<S: ib_inject::PenSink + ?Sized> CircleMover<S> {
    /// Time between moves: 500 per second, above any refresh rate the compositor can pace to.
    const STEP: Duration = Duration::from_millis(2);
    /// One turn per second.
    const TURNS_PER_S: f64 = 1.0;

    fn new(sink: Box<S>, display: ib_geometry::Region) -> Self {
        Self {
            sink,
            centre: (
                f64::from(display.width) / 2.0,
                f64::from(display.height) / 2.0,
            ),
            radius: f64::from(display.height) / 6.0,
            started: Instant::now(),
            last: None,
        }
    }

    /// Where the pointer is `elapsed` after the start.
    fn point_at(&self, elapsed: Duration) -> ib_geometry::DesktopPoint {
        let angle = std::f64::consts::TAU * Self::TURNS_PER_S * elapsed.as_secs_f64();
        ib_geometry::DesktopPoint {
            x: self.centre.0 + self.radius * angle.cos(),
            y: self.centre.1 + self.radius * angle.sin(),
        }
    }

    /// Moves the pointer if a step's time has passed since the last move.
    fn step(&mut self) -> Result<(), ib_inject::InjectError> {
        let now = Instant::now();
        if self
            .last
            .is_some_and(|t| now.duration_since(t) < Self::STEP)
        {
            return Ok(());
        }
        self.last = Some(now);
        let at = self.point_at(now.duration_since(self.started));
        self.sink.move_pointer(at)
    }
}

/// Real desktop frames through the hardware encoder: catches stride and
/// pixel-format mistakes that synthetic frames can't.
fn encode_captured(r: &mut Report, frames: &[Frame]) {
    use ib_encode::EncoderConfig;
    use ib_protocol::VideoCodec;

    let last = frames.last().expect("not empty");
    let config = EncoderConfig {
        codec: VideoCodec::H264,
        width: last.width,
        height: last.height,
        fps: 60,
        bitrate_kbps: 25_000,
    };
    match ib_encode::VaapiEncoder::new(config) {
        Ok(mut enc) => match encode_and_check(&mut enc, frames) {
            Ok((mut times, _)) => r.pass(&format!(
                "captured frames encode with VA-API and decode with the right colours ({})",
                timing(&mut times)
            )),
            Err(e) => r.fail(&format!("encoding captured frames: {e}")),
        },
        Err(e) => r.info(&format!("VA-API check of captured frames skipped: {e}")),
    }
}

fn pointer(r: &mut Report, desktop: &crate::wayland::WaylandDesktop, display: ib_geometry::Region) {
    use ib_geometry::DesktopPoint;

    r.section("Wayland pointer (remote desktop portal)");
    let Some(mut sink) = desktop.pointer_sink() else {
        r.fail("pointer control was not granted");
        return;
    };
    println!("\n>>> Watch the mouse pointer: it will draw a square in the middle of the screen.");
    std::thread::sleep(Duration::from_secs(2));
    let (cx, cy) = (
        f64::from(display.width) / 2.0,
        f64::from(display.height) / 2.0,
    );
    let half = f64::from(display.height) / 6.0;
    let corners = [
        (-1.0, -1.0),
        (1.0, -1.0),
        (1.0, 1.0),
        (-1.0, 1.0),
        (-1.0, -1.0),
    ];
    for pair in corners.windows(2) {
        let ((ax, ay), (bx, by)) = (pair[0], pair[1]);
        for step in 0..=30 {
            let t = f64::from(step) / 30.0;
            let at = DesktopPoint {
                x: cx + half * (ax + (bx - ax) * t),
                y: cy + half * (ay + (by - ay) * t),
            };
            if let Err(e) = sink.move_pointer(at) {
                r.fail(&format!("moving the pointer: {e}"));
                return;
            }
            std::thread::sleep(Duration::from_millis(15));
        }
    }
    match ask("Did the pointer move around a square in the middle of the screen?") {
        Some(true) => r.pass("pointer moves where expected"),
        Some(false) => r.fail("pointer did not move as expected (user)"),
        None => {
            r.info(
                "pointer moved without errors; not confirmed (no one at the terminal to answer)",
            );
        }
    }
}

fn uinput(r: &mut Report) {
    r.section("Pressure pen (uinput, Advanced option)");
    let probe = ib_geometry::Region {
        x: 0,
        y: 0,
        width: 1920,
        height: 1080,
    };
    match ib_inject::UinputPenSink::new(probe) {
        Ok(_) => r.pass("virtual pressure tablet can be created"),
        Err(e) => r.info(&format!("not set up: {e}")),
    }
}

/// Asks a yes/no question on the terminal (anything but yes is no).
/// `None` when no one can answer: stdin isn't a terminal or is closed, as
/// when the self-test runs from a script or an agent.
fn ask(question: &str) -> Option<bool> {
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() {
        return None;
    }
    print!("\n>>> {question} [y/n] ");
    let _ = std::io::stdout().flush();
    let mut answer = String::new();
    match std::io::stdin().lock().read_line(&mut answer) {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(is_yes(&answer)),
    }
}

fn is_yes(answer: &str) -> bool {
    matches!(answer.trim(), "y" | "Y" | "yes")
}

/// Saves a frame as a binary PPM image (half size), for a visual check of
/// colours and alignment.
fn save_ppm(frame: &Frame, path: &Path) -> std::io::Result<PathBuf> {
    let FrameData::Cpu(px) = &frame.data else {
        return Err(std::io::Error::other("not a CPU frame"));
    };
    let (w, h) = (frame.width as usize / 2, frame.height as usize / 2);
    let mut out = std::io::BufWriter::new(File::create(path)?);
    write!(out, "P6\n{w} {h}\n255\n")?;
    for y in 0..h {
        for x in 0..w {
            let i = (2 * y * frame.width as usize + 2 * x) * 4;
            out.write_all(&[px[i + 2], px[i + 1], px[i]])?;
        }
    }
    out.flush()?;
    Ok(path.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DISPLAY: ib_geometry::Region = ib_geometry::Region {
        x: 0,
        y: 0,
        width: 2560,
        height: 1440,
    };

    #[test]
    fn circle_mover_goes_round_the_middle_of_the_display() {
        let m = CircleMover::new(Box::new(ib_inject::FakePenSink::default()), DISPLAY);
        let start = m.point_at(Duration::ZERO);
        assert!((start.x - 1520.0).abs() < 1e-9 && (start.y - 720.0).abs() < 1e-9);
        let quarter = m.point_at(Duration::from_millis(250));
        assert!((quarter.x - 1280.0).abs() < 1e-6 && (quarter.y - 960.0).abs() < 1e-6);
        let turn = m.point_at(Duration::from_secs(1));
        assert!((turn.x - start.x).abs() < 1e-6 && (turn.y - start.y).abs() < 1e-6);
    }

    #[test]
    fn circle_mover_moves_at_most_once_per_step() {
        let mut m = CircleMover::new(Box::new(ib_inject::FakePenSink::default()), DISPLAY);
        m.step().unwrap();
        m.step().unwrap();
        assert_eq!(m.sink.pointer_moves.len(), 1);
        std::thread::sleep(CircleMover::<ib_inject::FakePenSink>::STEP);
        m.step().unwrap();
        assert_eq!(m.sink.pointer_moves.len(), 2);
    }

    #[test]
    fn only_yes_counts_as_yes() {
        assert!(is_yes("y\n"));
        assert!(is_yes(" Y "));
        assert!(is_yes("yes"));
        assert!(!is_yes("n"));
        assert!(!is_yes(""));
        assert!(!is_yes("yep"));
    }

    #[test]
    fn luma_matches_the_encoder_conversion() {
        let mut y = [0u8; 4];
        let mut uv = [0u8; 2];
        let px = [30u8, 140, 220, 255].repeat(4);
        ib_encode::nv12::bgra_to_nv12(
            &px,
            2,
            2,
            &mut ib_encode::nv12::Nv12Planes {
                y: &mut y,
                y_pitch: 2,
                uv: &mut uv,
                uv_pitch: 2,
            },
        );
        assert_eq!(u32::from(y[0]), luma(&px));
    }
}
