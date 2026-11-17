//! macOS hardware H.264 with VideoToolbox, fed by pixel buffers straight
//! from ScreenCaptureKit (no CPU copy).
//!
//! Configured for latency: low-latency rate control, real-time, no frame
//! reordering, no periodic keyframes. `encode` submits one frame and waits
//! for its output, so nothing queues inside the encoder (ADR-0005).
//!
//! Low-latency rate control holds the bitrate by skipping frames when the
//! picture changes faster than the bitrate allows. `encode` reports that as
//! [`EncodeError::Dropped`]; the stream stays valid and the caller sends the
//! frame again later.
//!
//! VideoToolbox emits AVCC (length-prefixed NAL units, parameter sets out of
//! band). The tablet's decoder wants Annex B with SPS and PPS in front of
//! every keyframe, so the output is converted here.

use std::ffi::c_void;
use std::ptr::{self, NonNull};
use std::sync::{Condvar, Mutex};
use std::time::Duration;

use ib_capture::{Frame, FrameData};
use ib_protocol::FrameKind;
use objc2_core_foundation::{CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType};
use objc2_core_media::{
    CMFormatDescription, CMSampleBuffer, CMTime,
    CMVideoFormatDescriptionGetH264ParameterSetAtIndex, kCMTimeInvalid, kCMVideoCodecType_H264,
};
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferPool, kCVPixelBufferHeightKey,
    kCVPixelBufferIOSurfacePropertiesKey, kCVPixelBufferPixelFormatTypeKey, kCVPixelBufferWidthKey,
    kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
};
use objc2_video_toolbox::{
    VTCompressionSession, VTEncodeInfoFlags, VTPixelTransferSession, VTSessionSetProperty,
    kVTCompressionPropertyKey_AllowFrameReordering, kVTCompressionPropertyKey_AverageBitRate,
    kVTCompressionPropertyKey_ExpectedFrameRate, kVTCompressionPropertyKey_MaxFrameDelayCount,
    kVTCompressionPropertyKey_MaxKeyFrameInterval,
    kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality,
    kVTCompressionPropertyKey_ProfileLevel, kVTCompressionPropertyKey_RealTime,
    kVTCompressionPropertyKey_ReferenceBufferCount, kVTEncodeFrameOptionKey_ForceKeyFrame,
    kVTProfileLevel_H264_Main_AutoLevel, kVTVideoEncoderSpecification_EnableLowLatencyRateControl,
    kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder,
};

use crate::{EncodeError, EncodedFrame, EncoderConfig, VideoEncoder};

const START_CODE: [u8; 4] = [0, 0, 0, 1];
const NAL_IDR: u8 = 5;
const NAL_SPS: u8 = 7;
/// No periodic keyframes: the pipeline asks for one when it needs one.
const KEYFRAME_INTERVAL: i32 = 36_000;
/// How long one frame may take before the encoder counts as stuck.
const ENCODE_TIMEOUT: Duration = Duration::from_secs(2);

/// One encoded frame as Annex B and whether it is a keyframe, or `None`
/// when the encoder skipped the frame to hold the bitrate.
type Output = Result<Option<(Vec<u8>, bool)>, String>;

/// Where the output callback leaves the frame for `encode` to pick up.
#[derive(Default)]
struct Shared {
    output: Mutex<Option<Output>>,
    ready: Condvar,
}

/// Appends the NAL units of an AVCC buffer (each prefixed with a big-endian
/// length of `length_size` bytes) to `out` in Annex B form. Returns the NAL
/// unit types found, or `None` if the buffer is malformed.
fn avcc_to_annex_b(avcc: &[u8], length_size: usize, out: &mut Vec<u8>) -> Option<Vec<u8>> {
    if !(1..=4).contains(&length_size) {
        return None;
    }
    let mut types = Vec::new();
    let mut rest = avcc;
    while !rest.is_empty() {
        let (prefix, tail) = rest.split_at_checked(length_size)?;
        let len = prefix
            .iter()
            .fold(0usize, |n, b| (n << 8) | usize::from(*b));
        let (nal, tail) = tail.split_at_checked(len)?;
        types.push(nal.first()? & 0x1F);
        out.extend_from_slice(&START_CODE);
        out.extend_from_slice(nal);
        rest = tail;
    }
    Some(types)
}

/// SPS and PPS of `format` in Annex B form, and the size of the length
/// prefix its samples use.
fn parameter_sets(format: &CMFormatDescription) -> Option<(Vec<u8>, usize)> {
    let mut out = Vec::new();
    let mut count = 0usize;
    let mut length_size = 0i32;
    let mut index = 0;
    loop {
        let mut set: *const u8 = ptr::null();
        let mut size = 0usize;
        // SAFETY: all out-pointers are valid for writes; `format` is a valid
        // H.264 format description.
        let status = unsafe {
            CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                format,
                index,
                &raw mut set,
                &raw mut size,
                &raw mut count,
                &raw mut length_size,
            )
        };
        if status != 0 || set.is_null() {
            return None;
        }
        out.extend_from_slice(&START_CODE);
        // SAFETY: the format description owns `size` bytes at `set` and
        // outlives this call.
        out.extend_from_slice(unsafe { std::slice::from_raw_parts(set, size) });
        index += 1;
        if index >= count {
            break;
        }
    }
    Some((out, usize::try_from(length_size).ok()?))
}

/// Turns VideoToolbox's sample into an Annex B frame.
fn convert(sample: &CMSampleBuffer) -> Output {
    // SAFETY: `sample` is a valid sample buffer for the duration of the callback.
    let (format, block) = unsafe { (sample.format_description(), sample.data_buffer()) };
    let format = format.ok_or("encoded frame without a format description")?;
    let block = block.ok_or("encoded frame without data")?;
    // SAFETY: `block` is a valid block buffer.
    let len = unsafe { block.data_length() };
    let mut avcc = vec![0u8; len];
    if let Some(dest) = NonNull::new(avcc.as_mut_ptr().cast::<c_void>()) {
        // SAFETY: `dest` has room for the `len` bytes copied.
        let status = unsafe { block.copy_data_bytes(0, len, dest) };
        if status != 0 {
            return Err(format!("reading the encoded frame failed ({status})"));
        }
    }
    let (sets, length_size) = parameter_sets(&format).ok_or("encoder sent no SPS/PPS")?;
    let mut body = Vec::with_capacity(len + 16);
    let types = avcc_to_annex_b(&avcc, length_size, &mut body).ok_or("malformed encoder output")?;
    let key = types.contains(&NAL_IDR);
    if key && !types.contains(&NAL_SPS) {
        let mut data = sets;
        data.extend_from_slice(&body);
        return Ok(Some((data, true)));
    }
    Ok(Some((body, key)))
}

/// Called by VideoToolbox when a frame is done (on one of its threads).
unsafe extern "C-unwind" fn on_output(
    shared: *mut c_void,
    _frame: *mut c_void,
    status: i32,
    flags: VTEncodeInfoFlags,
    sample: *mut CMSampleBuffer,
) {
    // SAFETY: `shared` is the `Shared` the session was created with, which
    // outlives the session (see `Drop for VtEncoder`).
    let shared = unsafe { &*shared.cast::<Shared>() };
    // SAFETY: VideoToolbox passes either null or a valid sample buffer.
    let output = match unsafe { sample.as_ref() } {
        _ if status != 0 => Err(format!("VideoToolbox error {status}")),
        None if flags.contains(VTEncodeInfoFlags::FrameDropped) => Ok(None),
        None => Err("the encoder returned no data".into()),
        Some(sample) => convert(sample),
    };
    if let Ok(mut slot) = shared.output.lock() {
        *slot = Some(output);
        shared.ready.notify_one();
    }
}

fn check(status: i32, what: &str) -> Result<(), EncodeError> {
    if status == 0 {
        Ok(())
    } else {
        Err(EncodeError::Unavailable(format!(
            "{what} failed ({status})"
        )))
    }
}

fn dictionary(keys: &[&CFString], values: &[&CFType]) -> CFRetained<CFDictionary> {
    let typed = CFDictionary::<CFString, CFType>::from_slices(keys, values);
    // SAFETY: a typed dictionary is the same object as an untyped one.
    unsafe { CFRetained::cast_unchecked(typed) }
}

pub struct VtEncoder {
    session: CFRetained<VTCompressionSession>,
    /// Scales frames that are larger than the picture; made when first needed.
    scaler: Option<CFRetained<VTPixelTransferSession>>,
    force_key: CFRetained<CFDictionary>,
    shared: Box<Shared>,
    config: EncoderConfig,
    last_pts: i64,
}

// SAFETY: VideoToolbox sessions may be used from any thread; the encoder is
// used by one thread at a time (`&mut self`).
unsafe impl Send for VtEncoder {}

impl std::fmt::Debug for VtEncoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VtEncoder")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl VtEncoder {
    /// A hardware encoder producing `config.width`×`config.height`. Input
    /// frames of another size are scaled on the GPU. Fails when this Mac
    /// has no hardware H.264 encoder.
    pub fn new(config: EncoderConfig) -> Result<Self, EncodeError> {
        let size = |v: u32| {
            i32::try_from(v).map_err(|_| EncodeError::Unavailable("picture too large".into()))
        };
        let (width, height) = (size(config.width)?, size(config.height)?);
        let yes: &CFType = CFBoolean::new(true);
        // SAFETY: plain reads of constant strings exported by VideoToolbox
        // and CoreVideo.
        let (spec, buffers) = unsafe {
            (
                dictionary(
                    &[
                        kVTVideoEncoderSpecification_EnableLowLatencyRateControl,
                        kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder,
                    ],
                    &[yes, yes],
                ),
                // What the session's buffer pool (used for scaling) hands out.
                dictionary(
                    &[
                        kCVPixelBufferPixelFormatTypeKey,
                        kCVPixelBufferWidthKey,
                        kCVPixelBufferHeightKey,
                        kCVPixelBufferIOSurfacePropertiesKey,
                    ],
                    &[
                        &CFNumber::new_i64(i64::from(
                            kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
                        )),
                        &CFNumber::new_i32(width),
                        &CFNumber::new_i32(height),
                        &dictionary(&[], &[]),
                    ],
                ),
            )
        };
        let shared = Box::<Shared>::default();
        let mut session: *mut VTCompressionSession = ptr::null_mut();
        // SAFETY: the dictionaries are valid, `session` is valid for writes
        // and `shared` lives until the session is gone (see `Drop`).
        let status = unsafe {
            VTCompressionSession::create(
                None,
                width,
                height,
                kCMVideoCodecType_H264,
                Some(&spec),
                Some(&buffers),
                None,
                Some(on_output),
                (&raw const *shared).cast_mut().cast::<c_void>(),
                NonNull::from(&mut session),
            )
        };
        check(status, "creating the hardware encoder")?;
        let session = NonNull::new(session)
            .ok_or_else(|| EncodeError::Unavailable("no hardware H.264 encoder".into()))?;
        // SAFETY: `create` returned an owned (+1) session.
        let session = unsafe { CFRetained::from_raw(session) };
        // SAFETY: plain read of a constant string.
        let force_key = dictionary(&[unsafe { kVTEncodeFrameOptionKey_ForceKeyFrame }], &[yes]);
        let encoder = Self {
            session,
            scaler: None,
            force_key,
            shared,
            config,
            last_pts: -1,
        };
        encoder.configure()?;
        Ok(encoder)
    }

    fn set(&self, key: &CFString, value: &CFType) -> i32 {
        // SAFETY: valid session, key and value.
        unsafe { VTSessionSetProperty(&self.session, key, Some(value)) }
    }

    fn configure(&self) -> Result<(), EncodeError> {
        let yes: &CFType = CFBoolean::new(true);
        let no: &CFType = CFBoolean::new(false);
        // SAFETY: plain reads of constant strings exported by VideoToolbox.
        unsafe {
            check(
                self.set(kVTCompressionPropertyKey_RealTime, yes),
                "real-time mode",
            )?;
            check(
                self.set(kVTCompressionPropertyKey_AllowFrameReordering, no),
                "turning off frame reordering",
            )?;
            check(
                self.set(
                    kVTCompressionPropertyKey_MaxKeyFrameInterval,
                    &CFNumber::new_i32(KEYFRAME_INTERVAL),
                ),
                "keyframe interval",
            )?;
            // Main profile, like the Windows backend.
            check(
                self.set(
                    kVTCompressionPropertyKey_ProfileLevel,
                    kVTProfileLevel_H264_Main_AutoLevel,
                ),
                "H.264 profile",
            )?;
            // Hints that not every encoder accepts.
            // One reference frame, like the other backends: the decoder
            // then needs the fewest buffers and never waits to reorder.
            self.set(
                kVTCompressionPropertyKey_ReferenceBufferCount,
                &CFNumber::new_i32(1),
            );
            self.set(
                kVTCompressionPropertyKey_ExpectedFrameRate,
                &CFNumber::new_i32(i32::from(self.config.fps)),
            );
            self.set(
                kVTCompressionPropertyKey_MaxFrameDelayCount,
                &CFNumber::new_i32(0),
            );
            self.set(
                kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality,
                yes,
            );
        }
        check(self.apply_bitrate(), "bitrate")?;
        // SAFETY: valid session.
        check(
            unsafe { self.session.prepare_to_encode_frames() },
            "preparing the hardware encoder",
        )
    }

    fn apply_bitrate(&self) -> i32 {
        let bits = i64::from(self.config.bitrate_kbps) * 1000;
        // SAFETY: plain read of a constant string.
        self.set(
            unsafe { kVTCompressionPropertyKey_AverageBitRate },
            &CFNumber::new_i64(bits),
        )
    }

    /// `source` scaled to the encoder's size, in a buffer from its pool.
    fn scaled(&mut self, source: &CVPixelBuffer) -> Result<CFRetained<CVPixelBuffer>, EncodeError> {
        let failed = |what: &str, status: i32| EncodeError::Failed(format!("{what} ({status})"));
        let scaler = if let Some(s) = &self.scaler {
            s.clone()
        } else {
            let mut raw: *mut VTPixelTransferSession = ptr::null_mut();
            // SAFETY: `raw` is valid for writes.
            let status = unsafe { VTPixelTransferSession::create(None, NonNull::from(&mut raw)) };
            let raw = NonNull::new(raw).ok_or_else(|| failed("no scaler", status))?;
            // SAFETY: `create` returned an owned (+1) session.
            let scaler = unsafe { CFRetained::from_raw(raw) };
            self.scaler.insert(scaler).clone()
        };
        // SAFETY: valid session.
        let pool: CFRetained<CVPixelBufferPool> = unsafe { self.session.pixel_buffer_pool() }
            .ok_or_else(|| EncodeError::Failed("the encoder has no buffer pool".into()))?;
        let mut raw: *mut CVPixelBuffer = ptr::null_mut();
        // SAFETY: valid pool; `raw` is valid for writes.
        let status =
            unsafe { CVPixelBufferPool::create_pixel_buffer(None, &pool, NonNull::from(&mut raw)) };
        let raw = NonNull::new(raw).ok_or_else(|| failed("no buffer for scaling", status))?;
        // SAFETY: the pool returned an owned (+1) buffer.
        let target = unsafe { CFRetained::from_raw(raw) };
        // SAFETY: both buffers are valid; the target is ours alone.
        let status = unsafe { scaler.transfer_image(source, &target) };
        if status != 0 {
            return Err(failed("scaling the frame failed", status));
        }
        Ok(target)
    }
}

impl Drop for VtEncoder {
    fn drop(&mut self) {
        // SAFETY: valid session. Finishing pending frames and invalidating
        // it guarantees the callback no longer runs when `shared` is freed.
        unsafe {
            self.session.complete_frames(kCMTimeInvalid);
            self.session.invalidate();
            if let Some(scaler) = &self.scaler {
                scaler.invalidate();
            }
        }
    }
}

impl VideoEncoder for VtEncoder {
    fn name(&self) -> &'static str {
        "videotoolbox"
    }

    fn config(&self) -> EncoderConfig {
        self.config
    }

    fn encode(&mut self, frame: &Frame, force_keyframe: bool) -> Result<EncodedFrame, EncodeError> {
        let FrameData::Gpu(gpu) = &frame.data else {
            return Err(EncodeError::UnsupportedFrame(
                "VideoToolbox path needs GPU frames".into(),
            ));
        };
        let scaled;
        let buffer: &CVPixelBuffer =
            if (frame.width, frame.height) == (self.config.width, self.config.height) {
                &gpu.buffer
            } else {
                scaled = self.scaled(&gpu.buffer)?;
                &scaled
            };
        // Timestamps must grow, also when a frame is encoded a second time
        // (a keyframe of a static screen).
        let pts = i64::try_from(frame.capture_t_us)
            .unwrap_or(i64::MAX)
            .max(self.last_pts + 1);
        self.last_pts = pts;
        if let Ok(mut slot) = self.shared.output.lock() {
            *slot = None;
        }
        // SAFETY: valid session and buffer; the dictionary outlives the call.
        let status = unsafe {
            self.session.encode_frame(
                buffer,
                CMTime::new(pts, 1_000_000),
                kCMTimeInvalid,
                force_keyframe.then_some(&*self.force_key),
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        if status != 0 {
            return Err(EncodeError::Failed(format!(
                "VideoToolbox rejected the frame ({status})"
            )));
        }
        // Low-latency rate control emits each frame at once; this makes
        // sure of it, so there is never a frame left inside the encoder.
        // SAFETY: valid session.
        unsafe { self.session.complete_frames(kCMTimeInvalid) };

        let stuck = || EncodeError::Failed("the hardware encoder stopped answering".into());
        let slot = self.shared.output.lock().map_err(|_| stuck())?;
        let (mut slot, _) = self
            .shared
            .ready
            .wait_timeout_while(slot, ENCODE_TIMEOUT, |o| o.is_none())
            .map_err(|_| stuck())?;
        let (data, key) = slot
            .take()
            .ok_or_else(stuck)?
            .map_err(EncodeError::Failed)?
            .ok_or(EncodeError::Dropped)?;
        Ok(EncodedFrame {
            kind: if key {
                FrameKind::Key
            } else {
                FrameKind::Delta
            },
            capture_t_us: frame.capture_t_us,
            data,
        })
    }

    fn set_bitrate(&mut self, kbps: u32) {
        self.config.bitrate_kbps = kbps;
        self.apply_bitrate();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::h264::nal_types;
    use ib_capture::GpuFrame;
    use ib_protocol::VideoCodec;
    use objc2_core_video::{
        CVPixelBufferCreate, CVPixelBufferGetBaseAddressOfPlane,
        CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
        CVPixelBufferUnlockBaseAddress,
    };
    use openh264::decoder::Decoder;
    use openh264::formats::YUVSource;

    #[test]
    fn avcc_becomes_annex_b() {
        // Two NAL units: an IDR slice (type 5) and a non-IDR slice (type 1).
        let avcc = [0, 0, 0, 2, 0x65, 0xAA, 0, 0, 0, 3, 0x41, 0xBB, 0xCC];
        let mut out = Vec::new();
        let types = avcc_to_annex_b(&avcc, 4, &mut out).unwrap();
        assert_eq!(types, [5, 1]);
        assert_eq!(out, [0, 0, 0, 1, 0x65, 0xAA, 0, 0, 0, 1, 0x41, 0xBB, 0xCC]);
        assert_eq!(nal_types(&out), [5, 1]);
    }

    #[test]
    fn short_length_prefixes_work() {
        let mut out = Vec::new();
        assert_eq!(
            avcc_to_annex_b(&[0, 1, 0x67, 0, 2, 0x68, 0x01], 2, &mut out).unwrap(),
            [7, 8]
        );
        assert_eq!(out, [0, 0, 0, 1, 0x67, 0, 0, 0, 1, 0x68, 0x01]);
    }

    #[test]
    fn malformed_avcc_is_rejected() {
        let mut out = Vec::new();
        // Length says 9 bytes, only 2 follow.
        assert!(avcc_to_annex_b(&[0, 0, 0, 9, 0x65, 0xAA], 4, &mut out).is_none());
        // Truncated length prefix.
        assert!(avcc_to_annex_b(&[0, 0], 4, &mut out).is_none());
        // Empty NAL unit and impossible prefix sizes.
        assert!(avcc_to_annex_b(&[0, 0, 0, 0], 4, &mut out).is_none());
        assert!(avcc_to_annex_b(&[1, 0x65], 0, &mut out).is_none());
        assert!(avcc_to_annex_b(&[1, 0x65], 5, &mut out).is_none());
    }

    /// An NV12 buffer whose left half is dark and right half is bright,
    /// with `shift` moving the edge so consecutive frames differ.
    fn split_frame(width: u32, height: u32, shift: u32) -> Frame {
        let attrs = dictionary(
            // SAFETY: plain read of a constant string.
            &[unsafe { kCVPixelBufferIOSurfacePropertiesKey }],
            &[&dictionary(&[], &[])],
        );
        let mut raw: *mut CVPixelBuffer = ptr::null_mut();
        // SAFETY: valid attributes; `raw` is valid for writes.
        let status = unsafe {
            CVPixelBufferCreate(
                None,
                width as usize,
                height as usize,
                kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
                Some(&attrs),
                NonNull::from(&mut raw),
            )
        };
        assert_eq!(status, 0);
        // SAFETY: `CVPixelBufferCreate` returned an owned (+1) buffer.
        let buffer = unsafe { CFRetained::from_raw(NonNull::new(raw).unwrap()) };
        let flags = CVPixelBufferLockFlags::empty();
        // SAFETY: the buffer is ours; locked before and unlocked after writing.
        unsafe {
            assert_eq!(CVPixelBufferLockBaseAddress(&buffer, flags), 0);
            let edge = (width / 2 + shift) as usize;
            for (plane, rows, dark, bright) in [(0, height, 30u8, 220u8), (1, height / 2, 128, 128)]
            {
                let base = CVPixelBufferGetBaseAddressOfPlane(&buffer, plane).cast::<u8>();
                let stride = CVPixelBufferGetBytesPerRowOfPlane(&buffer, plane);
                for y in 0..rows as usize {
                    let row = std::slice::from_raw_parts_mut(base.add(y * stride), width as usize);
                    for (x, px) in row.iter_mut().enumerate() {
                        *px = if x < edge { dark } else { bright };
                    }
                }
            }
            CVPixelBufferUnlockBaseAddress(&buffer, flags);
        }
        Frame {
            capture_t_us: u64::from(shift) * 16_667,
            width,
            height,
            data: FrameData::Gpu(GpuFrame { buffer }),
        }
    }

    /// A bitrate far too low for the content makes the encoder skip frames.
    /// That must be reported as `Dropped`, and the frames that do come out
    /// must still decode. Needs a hardware H.264 encoder:
    /// `cargo test -p ib-encode -- --ignored --nocapture skipped_frames_are_not_failures`
    #[test]
    #[ignore = "needs a hardware H.264 encoder"]
    fn skipped_frames_are_not_failures() {
        let config = EncoderConfig {
            codec: VideoCodec::H264,
            width: 1680,
            height: 1088,
            fps: 60,
            bitrate_kbps: 300,
        };
        let mut encoder = VtEncoder::new(config).expect("hardware encoder");
        let mut decoder = Decoder::new().unwrap();
        let (mut dropped, mut decoded) = (0, 0);
        for i in 0..90u32 {
            // The edge jumps across the picture: a lot changes every frame.
            let frame = split_frame(1680, 1088, (i * 97) % 700);
            let frame = Frame {
                capture_t_us: u64::from(i) * 16_667,
                ..frame
            };
            match encoder.encode(&frame, i == 0) {
                Ok(out) => {
                    assert!(decoder.decode(&out.data).expect("valid H.264").is_some());
                    decoded += 1;
                }
                Err(EncodeError::Dropped) => dropped += 1,
                Err(e) => panic!("frame {i}: {e}"),
            }
        }
        println!("{decoded} encoded, {dropped} skipped");
        assert!(decoded > 0);
    }

    /// Encodes synthetic frames on the real hardware encoder, scaled down,
    /// and decodes them with openh264. Needs a Mac with a hardware H.264
    /// encoder (any Apple silicon Mac), no permissions:
    /// `cargo test -p ib-encode -- --ignored --nocapture hardware_encodes_scaled_frames`
    #[test]
    #[ignore = "needs a hardware H.264 encoder"]
    fn hardware_encodes_scaled_frames() {
        let config = EncoderConfig {
            codec: VideoCodec::H264,
            width: 960,
            height: 608,
            fps: 60,
            bitrate_kbps: 8_000,
        };
        let mut encoder = VtEncoder::new(config).expect("hardware encoder");
        let mut decoder = Decoder::new().unwrap();
        let mut times = Vec::new();
        for i in 0..30u32 {
            let frame = split_frame(1920, 1216, i * 4);
            let key = i == 0 || i == 15;
            let started = std::time::Instant::now();
            let out = encoder.encode(&frame, key).expect("encode");
            times.push(started.elapsed());
            let types = nal_types(&out.data);
            if key {
                assert_eq!(out.kind, FrameKind::Key, "frame {i}");
                assert!(types.starts_with(&[7, 8]), "SPS/PPS first, got {types:?}");
            } else {
                assert_eq!(out.kind, FrameKind::Delta, "frame {i}");
            }
            assert_eq!(out.capture_t_us, frame.capture_t_us);
            // One frame in, the same frame out: the decoder has a picture
            // for every input.
            let pic = decoder
                .decode(&out.data)
                .expect("valid H.264")
                .expect("a picture for every frame");
            assert_eq!(pic.dimensions(), (960, 608));
            // The whole source is in the picture (scaled, not cropped).
            let (y, stride) = (pic.y(), pic.strides().0);
            let mid = 300 * stride;
            assert!(y[mid + 40] < 80, "left side dark, got {}", y[mid + 40]);
            assert!(
                y[mid + 920] > 170,
                "right side bright, got {}",
                y[mid + 920]
            );
            if i == 20 {
                encoder.set_bitrate(3_000);
                assert_eq!(encoder.config().bitrate_kbps, 3_000);
            }
        }
        times.sort();
        println!(
            "encode with scaling: median {:.2} ms, max {:.2} ms",
            times[15].as_secs_f64() * 1000.0,
            times[29].as_secs_f64() * 1000.0
        );
    }
}
