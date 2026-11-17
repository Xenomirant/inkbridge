//! Capture and hardware-encoder probe for a real Mac (needs a display and
//! the Screen Recording permission for the terminal app, so it's ignored by
//! default):
//!
//! ```text
//! cargo test --release -p ib-host --test mac_pipeline_probe -- --ignored --nocapture
//! ```
//!
//! Captures the main display with ScreenCaptureKit as GPU pixel buffers,
//! scaled by the system to the size a tablet would get,
//! encodes them with VideoToolbox, decodes
//! every frame with an independent decoder (openh264) to prove the bitstream
//! is valid, and reports encode times.
#![cfg(target_os = "macos")]

use std::time::{Duration, Instant};

use ib_capture::{FrameData, MacScreenSource, ScreenSource};
use ib_encode::{EncoderConfig, VideoEncoder, VtEncoder, fit_within};
use ib_protocol::{FrameKind, VideoCodec};
use openh264::decoder::Decoder;
use openh264::formats::YUVSource;

#[test]
#[ignore = "needs a real display and the Screen Recording permission"]
fn gpu_capture_hardware_encode_decodes_cleanly() {
    let mut source = MacScreenSource::new_gpu().expect("Screen Recording permission");
    let display = source.displays().expect("displays")[0].bounds;
    // The TB331FC tablet decodes up to 1920x1088; the system scales to that.
    let (width, height) = fit_within(display.width, display.height, 1920, 1088);
    source.set_output_size(Some((width, height)));
    source.start(display).expect("start capture");
    let frame = source
        .next_frame(Duration::from_secs(3))
        .expect("capture")
        .expect("a first frame");
    assert!(
        matches!(frame.data, FrameData::Gpu(_)),
        "expected a GPU frame"
    );

    assert_eq!((frame.width, frame.height), (width, height));
    let config = EncoderConfig {
        codec: VideoCodec::H264,
        width,
        height,
        fps: 60,
        bitrate_kbps: 18_000,
    };
    let mut encoder = VtEncoder::new(config).expect("hardware encoder");
    println!(
        "display {}x{} -> capture and encode {width}x{height}",
        display.width, display.height
    );

    let mut decoder = Decoder::new().expect("openh264 decoder");
    let mut times = Vec::new();
    let mut fresh = 0;
    for i in 0..120 {
        // Use fresh frames when the screen changes; otherwise re-encode the last.
        let f = match source.next_frame(Duration::from_millis(5)).ok().flatten() {
            Some(f) => {
                fresh += 1;
                f
            }
            None => frame.clone(),
        };
        let key = i == 0 || i == 60;
        let started = Instant::now();
        let out = encoder.encode(&f, key).expect("encode");
        times.push(started.elapsed());
        if key {
            assert_eq!(out.kind, FrameKind::Key, "frame {i} must be a keyframe");
        }
        let pic = decoder
            .decode(&out.data)
            .expect("valid H.264")
            .expect("a picture for every frame");
        let (w, h) = pic.dimensions();
        assert_eq!((w as u32, h as u32), (width, height));
    }
    times.sort();
    let ms = |d: Duration| d.as_secs_f64() * 1000.0;
    println!(
        "encode time: median {:.2} ms, p95 {:.2} ms, max {:.2} ms; {fresh}/120 fresh frames",
        ms(times[60]),
        ms(times[113]),
        ms(times[119])
    );
}
