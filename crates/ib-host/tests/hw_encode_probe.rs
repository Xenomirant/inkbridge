//! Hardware-encoder probe for a real Windows machine (needs a display and a
//! GPU, so it's ignored by default):
//!
//! ```text
//! cargo test -p ib-host --test hw_encode_probe -- --ignored --nocapture
//! ```
//!
//! Captures the primary display on the GPU, encodes frames with the Media
//! Foundation hardware encoder, decodes every one with an independent decoder
//! (openh264) to prove the bitstream is valid, and reports encode times.
#![cfg(windows)]

use std::time::{Duration, Instant};

use ib_capture::{ScreenSource, WindowsScreenSource};
use ib_encode::{EncoderConfig, MfEncoder, VideoEncoder, fit_within};
use ib_protocol::{FrameKind, VideoCodec};
use openh264::decoder::Decoder;
use openh264::formats::YUVSource;

#[test]
#[ignore = "needs a real display and GPU"]
fn gpu_capture_hardware_encode_decodes_cleanly() {
    let mut source = WindowsScreenSource::new_gpu();
    let display = source.displays().expect("displays")[0].bounds;
    source.start(display).expect("start capture");
    let frame = source
        .next_frame(Duration::from_secs(3))
        .expect("capture")
        .expect("a first frame");
    let ib_capture::FrameData::Gpu(gpu) = &frame.data else {
        panic!("expected a GPU frame")
    };

    let (width, height) = fit_within(frame.width, frame.height, 1920, 1920);
    let config = EncoderConfig {
        codec: VideoCodec::H264,
        width,
        height,
        fps: 60,
        bitrate_kbps: 20_000,
    };
    let mut encoder =
        MfEncoder::new(&gpu.device, (frame.width, frame.height), config).expect("hardware encoder");
    println!(
        "encoder: {} | capture {}x{} -> encode {}x{}",
        encoder.encoder_name(),
        frame.width,
        frame.height,
        width,
        height
    );

    let mut decoder = Decoder::new().expect("openh264 decoder");
    let mut times = Vec::new();
    let mut decoded = 0;
    for i in 0..60 {
        // Use fresh frames when the screen changes; otherwise re-encode the last.
        let f = source
            .next_frame(Duration::from_millis(5))
            .ok()
            .flatten()
            .unwrap_or_else(|| frame.clone());
        let started = Instant::now();
        let out = encoder.encode(&f, i == 0 || i == 30).expect("encode");
        times.push(started.elapsed());
        if i == 0 || i == 30 {
            assert_eq!(out.kind, FrameKind::Key, "frame {i} must be a keyframe");
        }
        if let Some(pic) = decoder.decode(&out.data).expect("valid H.264") {
            let (w, h) = pic.dimensions();
            assert_eq!((w as u32, h as u32), (width, height));
            decoded += 1;
        }
    }
    times.sort();
    let ms = |d: Duration| d.as_secs_f64() * 1000.0;
    println!(
        "encode time: median {:.2} ms, p95 {:.2} ms, max {:.2} ms; decoded {decoded}/60",
        ms(times[30]),
        ms(times[57]),
        ms(times[59])
    );
    assert!(decoded >= 55, "most frames must decode");
}
