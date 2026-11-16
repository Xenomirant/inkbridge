//! Linux X11 pipeline probe (needs an X server, so ignored by default):
//!
//! ```text
//! cargo test -p ib-host --test linux_pipeline_probe -- --ignored --nocapture
//! ```
//!
//! Real X11 capture → the production video pipeline (software H.264) →
//! every chunk decoded by an independent decoder.
#![cfg(target_os = "linux")]

use std::time::{Duration, Instant};

use ib_capture::{ScreenSource, X11ScreenSource};
use ib_host::video::VideoPipeline;
use ib_protocol::FrameKind;
use openh264::decoder::Decoder;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs an X server"]
async fn x11_capture_streams_decodable_video() {
    let source = X11ScreenSource::connect().expect("X server");
    let display = source.displays().unwrap()[0].bounds;
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let pipeline =
        VideoPipeline::start_with(Box::new(source), None, display, (1920, 1920), tx).unwrap();
    pipeline.request_keyframe();

    let mut decoder = Decoder::new().unwrap();
    let started = Instant::now();
    let mut got = 0;
    // A static screen sends little: request keyframes to get several frames.
    while got < 5 && started.elapsed() < Duration::from_secs(10) {
        if let Ok(Some(chunk)) = tokio::time::timeout(Duration::from_millis(500), rx.recv()).await {
            if got == 0 {
                assert_eq!(chunk.kind, FrameKind::Key);
                println!(
                    "display {display:?} -> stream {}x{}",
                    chunk.width, chunk.height
                );
            }
            decoder.decode(&chunk.payload).expect("valid H.264");
            got += 1;
        }
        pipeline.request_keyframe();
    }
    assert!(got >= 3, "expected several decodable frames, got {got}");
}
