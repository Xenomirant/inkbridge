//! Linux hardware H.264 encoding through VA-API (Intel iHD/i965, AMD
//! radeonsi, NVIDIA via Mesa's d3d12 driver on WSL…).
//!
//! Low-latency configuration, like every Inkbridge encoder (ADR-0005):
//! I/P frames only, one reference frame, CBR with a ~2-frame HRD buffer, no
//! periodic keyframes (the client asks for one after a loss), and each frame
//! returned before the next is accepted.
//!
//! Frames arrive as CPU BGRA and are converted to NV12 straight into the
//! encoder's input surface. DMA-BUF import from PipeWire (zero-copy) is a
//! later optimisation.
//!
//! Parameter sets come from [`crate::h264`]: sent as packed headers when the
//! driver accepts them, otherwise taken from the driver's own output, and
//! prepended as a last resort if a keyframe arrives without them.

mod ffi;

use std::ffi::{CStr, c_void};
use std::os::fd::{AsRawFd, OwnedFd};
use std::ptr;

use ib_capture::{Frame, FrameData};
use ib_protocol::{FrameKind, VideoCodec};

use crate::h264::{self, Profile, SeqParams};
use crate::nv12::{Nv12Planes, bgra_to_nv12};
use crate::{EncodeError, EncodedFrame, EncoderConfig, VideoEncoder};

fn unavailable(what: &str, status: ffi::VAStatus) -> EncodeError {
    EncodeError::Unavailable(format!("{what}: {}", status_str(status)))
}

fn failed(what: &str, status: ffi::VAStatus) -> EncodeError {
    EncodeError::Failed(format!("{what}: {}", status_str(status)))
}

fn status_str(status: ffi::VAStatus) -> String {
    // SAFETY: vaErrorStr returns a static string for any status.
    let s = unsafe { ffi::vaErrorStr(status) };
    if s.is_null() {
        format!("VA status {status}")
    } else {
        // SAFETY: non-null, NUL-terminated static string.
        unsafe { CStr::from_ptr(s) }.to_string_lossy().into_owned()
    }
}

const OK: ffi::VAStatus = ffi::VA_STATUS_SUCCESS.cast_signed();

/// An initialised VA display on a DRM render node.
struct Display {
    dpy: ffi::VADisplay,
    _fd: OwnedFd,
    vendor: String,
}

impl Display {
    /// Opens the first render node whose driver initialises.
    fn open() -> Result<Self, EncodeError> {
        let mut last = String::from("no /dev/dri/renderD* device");
        for n in 128..136 {
            let path = format!("/dev/dri/renderD{n}");
            let fd: OwnedFd = match std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
            {
                Ok(f) => f.into(),
                Err(e) => {
                    if e.kind() != std::io::ErrorKind::NotFound {
                        last = format!("{path}: {e}");
                    }
                    continue;
                }
            };
            // SAFETY: fd stays open for the display's lifetime (kept in Self).
            let dpy = unsafe { ffi::vaGetDisplayDRM(fd.as_raw_fd()) };
            if dpy.is_null() {
                continue;
            }
            let (mut major, mut minor) = (0, 0);
            // SAFETY: valid display; out-pointers are locals.
            let st = unsafe { ffi::vaInitialize(dpy, &raw mut major, &raw mut minor) };
            if st != OK {
                last = format!("{path}: {}", status_str(st));
                // SAFETY: terminating the display we just created.
                unsafe { ffi::vaTerminate(dpy) };
                continue;
            }
            // SAFETY: initialised display; static string.
            let vendor = unsafe {
                let v = ffi::vaQueryVendorString(dpy);
                if v.is_null() {
                    String::new()
                } else {
                    CStr::from_ptr(v).to_string_lossy().into_owned()
                }
            };
            return Ok(Self {
                dpy,
                _fd: fd,
                vendor,
            });
        }
        Err(EncodeError::Unavailable(format!("VA-API: {last}")))
    }

    fn entrypoints(&self, profile: ffi::VAProfile) -> Vec<ffi::VAEntrypoint> {
        // SAFETY: valid display; buffer sized by vaMaxNumEntrypoints.
        unsafe {
            let max = usize::try_from(ffi::vaMaxNumEntrypoints(self.dpy)).unwrap_or(0);
            let mut list = vec![0; max.max(1)];
            let mut n = 0;
            if ffi::vaQueryConfigEntrypoints(self.dpy, profile, list.as_mut_ptr(), &raw mut n) != OK
            {
                return Vec::new();
            }
            list.truncate(usize::try_from(n).unwrap_or(0));
            list
        }
    }

    fn attribute(
        &self,
        profile: ffi::VAProfile,
        entry: ffi::VAEntrypoint,
        kind: ffi::VAConfigAttribType,
    ) -> Option<u32> {
        let mut a = ffi::VAConfigAttrib {
            type_: kind,
            value: 0,
        };
        // SAFETY: valid display; one attribute in/out.
        let st = unsafe { ffi::vaGetConfigAttributes(self.dpy, profile, entry, &raw mut a, 1) };
        (st == OK && a.value != ffi::VA_ATTRIB_NOT_SUPPORTED).then_some(a.value)
    }
}

impl Drop for Display {
    fn drop(&mut self) {
        // SAFETY: display initialised in `open`, terminated once.
        unsafe { ffi::vaTerminate(self.dpy) };
    }
}

/// Chooses the best supported H.264 profile and encode entrypoint.
fn pick_profile(d: &Display) -> Option<(Profile, ffi::VAProfile, ffi::VAEntrypoint)> {
    let candidates = [
        (Profile::High, ffi::VAProfile_VAProfileH264High),
        (Profile::Main, ffi::VAProfile_VAProfileH264Main),
        (
            Profile::ConstrainedBaseline,
            ffi::VAProfile_VAProfileH264ConstrainedBaseline,
        ),
    ];
    for (p, va) in candidates {
        let eps = d.entrypoints(va);
        // Full encoder first (better rate control), low-power second.
        for want in [
            ffi::VAEntrypoint_VAEntrypointEncSlice,
            ffi::VAEntrypoint_VAEntrypointEncSliceLP,
        ] {
            if eps.contains(&want) {
                return Some((p, va, want));
            }
        }
    }
    None
}

/// The reconstructed picture used as reference for the next frame.
#[derive(Clone, Copy)]
struct RefPic {
    surface: ffi::VASurfaceID,
    frame_num: u16,
}

pub struct VaapiEncoder {
    config: EncoderConfig,
    seq: SeqParams,
    display: Display,
    va_config: ffi::VAConfigID,
    context: ffi::VAContextID,
    input: ffi::VASurfaceID,
    /// Two reconstructed surfaces: the current picture and the reference.
    recon: [ffi::VASurfaceID; 2],
    coded: ffi::VABufferID,
    packed_headers: bool,
    reference: Option<RefPic>,
    next_recon: usize,
    frame_num: u16,
    idr_id: u16,
    bitrate_changed: bool,
}

impl std::fmt::Debug for VaapiEncoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VaapiEncoder")
            .field("config", &self.config)
            .field("vendor", &self.display.vendor)
            .field("profile", &self.seq.profile)
            .finish_non_exhaustive()
    }
}

// SAFETY: VA handles are only used through `&mut self`, so from one thread
// at a time, which libva supports.
unsafe impl Send for VaapiEncoder {}

impl VaapiEncoder {
    /// Opens the GPU's encoder for `config` (H.264, even dimensions).
    pub fn new(config: EncoderConfig) -> Result<Self, EncodeError> {
        if config.codec != VideoCodec::H264 {
            return Err(EncodeError::Unavailable(
                "VA-API backend encodes H.264 only".into(),
            ));
        }
        if !config.width.is_multiple_of(2) || !config.height.is_multiple_of(2) || config.width == 0
        {
            return Err(EncodeError::Unavailable(format!(
                "odd frame size {}x{}",
                config.width, config.height
            )));
        }
        let display = Display::open()?;
        let (profile, va_profile, entry) = pick_profile(&display).ok_or_else(|| {
            EncodeError::Unavailable(format!(
                "the VA-API driver ({}) has no H.264 encoder",
                display.vendor
            ))
        })?;
        let rc = display
            .attribute(
                va_profile,
                entry,
                ffi::VAConfigAttribType_VAConfigAttribRateControl,
            )
            .unwrap_or(0);
        let packed = display
            .attribute(
                va_profile,
                entry,
                ffi::VAConfigAttribType_VAConfigAttribEncPackedHeaders,
            )
            .unwrap_or(0);
        let want_packed = ffi::VA_ENC_PACKED_HEADER_SEQUENCE | ffi::VA_ENC_PACKED_HEADER_PICTURE;
        let packed_headers = packed & want_packed == want_packed;

        let mut attribs = vec![ffi::VAConfigAttrib {
            type_: ffi::VAConfigAttribType_VAConfigAttribRTFormat,
            value: ffi::VA_RT_FORMAT_YUV420,
        }];
        if rc & ffi::VA_RC_CBR != 0 {
            attribs.push(ffi::VAConfigAttrib {
                type_: ffi::VAConfigAttribType_VAConfigAttribRateControl,
                value: ffi::VA_RC_CBR,
            });
        }
        if packed_headers {
            attribs.push(ffi::VAConfigAttrib {
                type_: ffi::VAConfigAttribType_VAConfigAttribEncPackedHeaders,
                value: want_packed,
            });
        }
        let seq = SeqParams {
            profile,
            width: config.width,
            height: config.height,
            fps: u32::from(config.fps.max(1)),
        };
        let mut enc = Self {
            config,
            seq,
            display,
            va_config: ffi::VA_INVALID_ID,
            context: ffi::VA_INVALID_ID,
            input: ffi::VA_INVALID_SURFACE,
            recon: [ffi::VA_INVALID_SURFACE; 2],
            coded: ffi::VA_INVALID_ID,
            packed_headers,
            reference: None,
            next_recon: 0,
            frame_num: 0,
            idr_id: 0,
            bitrate_changed: true,
        };
        enc.create(va_profile, entry, &mut attribs)?;
        tracing::info!(
            "VA-API encoder: {} {:?} {}x{} (packed headers: {packed_headers})",
            enc.display.vendor,
            profile,
            config.width,
            config.height
        );
        Ok(enc)
    }

    fn create(
        &mut self,
        profile: ffi::VAProfile,
        entry: ffi::VAEntrypoint,
        attribs: &mut [ffi::VAConfigAttrib],
    ) -> Result<(), EncodeError> {
        let dpy = self.display.dpy;
        let n = i32::try_from(attribs.len()).unwrap_or(0);
        // SAFETY: valid display; attribute array and out-pointer are live.
        let st = unsafe {
            ffi::vaCreateConfig(
                dpy,
                profile,
                entry,
                attribs.as_mut_ptr(),
                n,
                &raw mut self.va_config,
            )
        };
        if st != OK {
            return Err(unavailable("vaCreateConfig", st));
        }
        let (aw, ah) = (self.seq.width_mbs() * 16, self.seq.height_mbs() * 16);
        let mut surfaces = [ffi::VA_INVALID_SURFACE; 3];
        // SAFETY: valid display; 3 surfaces written into the local array.
        let st = unsafe {
            ffi::vaCreateSurfaces(
                dpy,
                ffi::VA_RT_FORMAT_YUV420,
                aw,
                ah,
                surfaces.as_mut_ptr(),
                3,
                ptr::null_mut(),
                0,
            )
        };
        if st != OK {
            return Err(unavailable("vaCreateSurfaces", st));
        }
        self.input = surfaces[0];
        self.recon = [surfaces[1], surfaces[2]];
        // SAFETY: valid config; render targets are the surfaces above.
        let st = unsafe {
            ffi::vaCreateContext(
                dpy,
                self.va_config,
                i32::try_from(aw).unwrap_or(0),
                i32::try_from(ah).unwrap_or(0),
                ffi::VA_PROGRESSIVE.cast_signed(),
                surfaces.as_mut_ptr(),
                3,
                &raw mut self.context,
            )
        };
        if st != OK {
            return Err(unavailable("vaCreateContext", st));
        }
        // Worst case for one frame: about the raw size.
        let coded_size = aw * ah * 3 / 2 + 64 * 1024;
        // SAFETY: valid context; the driver allocates the coded buffer.
        let st = unsafe {
            ffi::vaCreateBuffer(
                dpy,
                self.context,
                ffi::VABufferType_VAEncCodedBufferType,
                coded_size,
                1,
                ptr::null_mut(),
                &raw mut self.coded,
            )
        };
        if st != OK {
            return Err(unavailable("coded buffer", st));
        }
        Ok(())
    }

    /// Creates a parameter buffer holding a copy of `bytes`.
    fn buffer(
        &self,
        kind: ffi::VABufferType,
        bytes: &[u8],
    ) -> Result<ffi::VABufferID, EncodeError> {
        let mut id = ffi::VA_INVALID_ID;
        let len = u32::try_from(bytes.len())
            .map_err(|_| EncodeError::Failed("parameter buffer too big".into()))?;
        // SAFETY: libva copies `len` bytes from `bytes` (only read).
        let st = unsafe {
            ffi::vaCreateBuffer(
                self.display.dpy,
                self.context,
                kind,
                len,
                1,
                bytes.as_ptr().cast_mut().cast(),
                &raw mut id,
            )
        };
        if st == OK {
            Ok(id)
        } else {
            Err(failed("vaCreateBuffer", st))
        }
    }

    fn struct_buffer<T: Copy>(
        &self,
        kind: ffi::VABufferType,
        v: &T,
    ) -> Result<ffi::VABufferID, EncodeError> {
        self.buffer(kind, as_bytes(v))
    }

    /// A misc parameter buffer: `{ u32 type; T payload }`.
    fn misc_buffer<T: Copy>(
        &self,
        misc_type: u32,
        payload: &T,
    ) -> Result<ffi::VABufferID, EncodeError> {
        let mut bytes = misc_type.to_ne_bytes().to_vec();
        bytes.extend_from_slice(as_bytes(payload));
        self.buffer(ffi::VABufferType_VAEncMiscParameterBufferType, &bytes)
    }

    /// Packed header (parameter + data buffer pair) for an SPS or PPS NAL.
    fn packed_buffers(
        &self,
        header_type: u32,
        nal: &[u8],
    ) -> Result<[ffi::VABufferID; 2], EncodeError> {
        let param = ffi::VAEncPackedHeaderParameterBuffer {
            type_: header_type,
            bit_length: u32::try_from(nal.len() * 8).unwrap_or(u32::MAX),
            has_emulation_bytes: 1,
            ..Default::default()
        };
        Ok([
            self.struct_buffer(
                ffi::VABufferType_VAEncPackedHeaderParameterBufferType,
                &param,
            )?,
            self.buffer(ffi::VABufferType_VAEncPackedHeaderDataBufferType, nal)?,
        ])
    }

    /// Converts the BGRA frame into the input surface.
    fn upload(&self, bgra: &[u8]) -> Result<(), EncodeError> {
        let dpy = self.display.dpy;
        let (w, h) = (self.config.width, self.config.height);
        let mut image = ffi::VAImage {
            image_id: ffi::VA_INVALID_ID,
            ..Default::default()
        };
        // Map the surface directly if the driver allows, else a staging image.
        // SAFETY: valid surface; image written by libva.
        let derived_ok = unsafe { ffi::vaDeriveImage(dpy, self.input, &raw mut image) } == OK;
        let derived = derived_ok && image.format.fourcc == ffi::VA_FOURCC_NV12;
        if !derived {
            if derived_ok {
                // SAFETY: destroying the unusable derived image.
                unsafe { ffi::vaDestroyImage(dpy, image.image_id) };
            }
            let mut fmt = ffi::VAImageFormat {
                fourcc: ffi::VA_FOURCC_NV12,
                byte_order: ffi::VA_LSB_FIRST,
                bits_per_pixel: 12,
                ..Default::default()
            };
            // SAFETY: format and out-image are locals.
            let st = unsafe {
                ffi::vaCreateImage(
                    dpy,
                    &raw mut fmt,
                    i32::try_from(w).unwrap_or(0),
                    i32::try_from(h).unwrap_or(0),
                    &raw mut image,
                )
            };
            if st != OK {
                return Err(failed("vaCreateImage", st));
            }
        }
        let result = self
            .fill_image(&image, bgra, w as usize, h as usize)
            .and_then(|()| {
                if derived {
                    return Ok(());
                }
                // SAFETY: image and surface are valid; copies w×h at the origin.
                let st = unsafe {
                    ffi::vaPutImage(dpy, self.input, image.image_id, 0, 0, w, h, 0, 0, w, h)
                };
                if st == OK {
                    Ok(())
                } else {
                    Err(failed("vaPutImage", st))
                }
            });
        // SAFETY: image created above, destroyed once.
        unsafe { ffi::vaDestroyImage(dpy, image.image_id) };
        result
    }

    fn fill_image(
        &self,
        image: &ffi::VAImage,
        bgra: &[u8],
        w: usize,
        h: usize,
    ) -> Result<(), EncodeError> {
        let dpy = self.display.dpy;
        let mut mapped: *mut c_void = ptr::null_mut();
        // SAFETY: the image's buffer is mappable for writing.
        let st = unsafe { ffi::vaMapBuffer(dpy, image.buf, &raw mut mapped) };
        if st != OK || mapped.is_null() {
            return Err(failed("vaMapBuffer(image)", st));
        }
        let size = image.data_size as usize;
        // SAFETY: libva maps `data_size` bytes until vaUnmapBuffer.
        let data = unsafe { std::slice::from_raw_parts_mut(mapped.cast::<u8>(), size) };
        let (y_off, uv_off) = (image.offsets[0] as usize, image.offsets[1] as usize);
        let (y_pitch, uv_pitch) = (image.pitches[0] as usize, image.pitches[1] as usize);
        let ok = uv_off >= y_off + y_pitch * h
            && uv_off + uv_pitch * (h / 2) <= size
            && y_pitch >= w
            && uv_pitch >= w;
        if ok {
            let (head, uv) = data.split_at_mut(uv_off);
            bgra_to_nv12(
                bgra,
                w,
                h,
                &mut Nv12Planes {
                    y: &mut head[y_off..],
                    y_pitch,
                    uv,
                    uv_pitch,
                },
            );
        }
        // SAFETY: unmapping what was mapped above.
        unsafe { ffi::vaUnmapBuffer(dpy, image.buf) };
        if ok {
            Ok(())
        } else {
            Err(EncodeError::Failed("unexpected VA image layout".into()))
        }
    }

    fn picture(surface: ffi::VASurfaceID, frame_num: u16, flags: u32) -> ffi::VAPictureH264 {
        ffi::VAPictureH264 {
            picture_id: surface,
            frame_idx: u32::from(frame_num),
            flags,
            TopFieldOrderCnt: i32::from(frame_num) * 2,
            BottomFieldOrderCnt: 0,
            ..Default::default()
        }
    }

    fn invalid_picture() -> ffi::VAPictureH264 {
        ffi::VAPictureH264 {
            picture_id: ffi::VA_INVALID_SURFACE,
            flags: ffi::VA_PICTURE_H264_INVALID,
            ..Default::default()
        }
    }

    fn reference_picture(&self, idr: bool) -> Option<ffi::VAPictureH264> {
        let r = self.reference.filter(|_| !idr)?;
        Some(Self::picture(
            r.surface,
            r.frame_num,
            ffi::VA_PICTURE_H264_SHORT_TERM_REFERENCE,
        ))
    }

    fn sequence_params(&self) -> ffi::VAEncSequenceParameterBufferH264 {
        let s = &self.seq;
        let mut p = ffi::VAEncSequenceParameterBufferH264 {
            seq_parameter_set_id: 0,
            level_idc: s.level_idc(),
            intra_period: 0,
            intra_idr_period: 0,
            ip_period: 1,
            bits_per_second: self.config.bitrate_kbps.saturating_mul(1000),
            max_num_ref_frames: 1,
            picture_width_in_mbs: u16::try_from(s.width_mbs()).unwrap_or(u16::MAX),
            picture_height_in_mbs: u16::try_from(s.height_mbs()).unwrap_or(u16::MAX),
            ..Default::default()
        };
        // SAFETY: writing the bitfield view of a plain-data union.
        unsafe {
            let f = &mut p.seq_fields.bits;
            f.set_chroma_format_idc(1);
            f.set_frame_mbs_only_flag(1);
            f.set_direct_8x8_inference_flag(1);
            f.set_log2_max_frame_num_minus4(h264::LOG2_MAX_FRAME_NUM_MINUS4);
            f.set_pic_order_cnt_type(2);
        }
        let (crop_r, crop_b) = s.crop();
        if crop_r != 0 || crop_b != 0 {
            p.frame_cropping_flag = 1;
            p.frame_crop_right_offset = crop_r;
            p.frame_crop_bottom_offset = crop_b;
        }
        p
    }

    fn picture_params(
        &self,
        idr: bool,
        recon: ffi::VASurfaceID,
    ) -> ffi::VAEncPictureParameterBufferH264 {
        let mut refs = [Self::invalid_picture(); 16];
        if let Some(r) = self.reference_picture(idr) {
            refs[0] = r;
        }
        let mut p = ffi::VAEncPictureParameterBufferH264 {
            CurrPic: Self::picture(recon, self.frame_num, 0),
            ReferenceFrames: refs,
            coded_buf: self.coded,
            frame_num: self.frame_num,
            pic_init_qp: 26,
            ..Default::default()
        };
        // SAFETY: writing the bitfield view of a plain-data union.
        unsafe {
            let f = &mut p.pic_fields.bits;
            f.set_idr_pic_flag(u32::from(idr));
            f.set_reference_pic_flag(1);
            f.set_entropy_coding_mode_flag(u32::from(self.seq.profile.cabac()));
            f.set_transform_8x8_mode_flag(u32::from(self.seq.profile.transform_8x8()));
            f.set_deblocking_filter_control_present_flag(1);
        }
        p
    }

    fn slice_params(&self, idr: bool) -> ffi::VAEncSliceParameterBufferH264 {
        let mut l0 = [Self::invalid_picture(); 32];
        if let Some(r) = self.reference_picture(idr) {
            l0[0] = r;
        }
        ffi::VAEncSliceParameterBufferH264 {
            macroblock_address: 0,
            num_macroblocks: self.seq.width_mbs() * self.seq.height_mbs(),
            macroblock_info: ffi::VA_INVALID_ID,
            slice_type: if idr { 2 } else { 0 }, // I or P
            idr_pic_id: self.idr_id,
            RefPicList0: l0,
            RefPicList1: [Self::invalid_picture(); 32],
            ..Default::default()
        }
    }

    fn rate_buffers(&self) -> Result<Vec<ffi::VABufferID>, EncodeError> {
        let bps = self.config.bitrate_kbps.saturating_mul(1000);
        let fps = u32::from(self.config.fps.max(1));
        let rc = ffi::VAEncMiscParameterRateControl {
            bits_per_second: bps,
            target_percentage: 100,
            window_size: 100, // ms: react quickly
            ..Default::default()
        };
        // About two frames of buffering: frame sizes stay even, so no frame
        // takes much longer than the others to send.
        let hrd = ffi::VAEncMiscParameterHRD {
            initial_buffer_fullness: bps / fps,
            buffer_size: bps / fps * 2,
            ..Default::default()
        };
        let rate = ffi::VAEncMiscParameterFrameRate {
            framerate: fps,
            ..Default::default()
        };
        Ok(vec![
            self.misc_buffer(
                ffi::VAEncMiscParameterType_VAEncMiscParameterTypeRateControl,
                &rc,
            )?,
            self.misc_buffer(ffi::VAEncMiscParameterType_VAEncMiscParameterTypeHRD, &hrd)?,
            self.misc_buffer(
                ffi::VAEncMiscParameterType_VAEncMiscParameterTypeFrameRate,
                &rate,
            )?,
        ])
    }

    fn parameter_buffers(
        &self,
        idr: bool,
        recon: ffi::VASurfaceID,
        buffers: &mut Vec<ffi::VABufferID>,
    ) -> Result<(), EncodeError> {
        if idr || self.bitrate_changed {
            buffers.push(self.struct_buffer(
                ffi::VABufferType_VAEncSequenceParameterBufferType,
                &self.sequence_params(),
            )?);
            buffers.extend(self.rate_buffers()?);
        }
        if idr && self.packed_headers {
            buffers.extend(
                self.packed_buffers(ffi::VA_ENC_PACKED_HEADER_SEQUENCE, &h264::sps(&self.seq))?,
            );
            buffers.extend(self.packed_buffers(
                ffi::VA_ENC_PACKED_HEADER_PICTURE,
                &h264::pps(self.seq.profile),
            )?);
        }
        buffers.push(self.struct_buffer(
            ffi::VABufferType_VAEncPictureParameterBufferType,
            &self.picture_params(idr, recon),
        )?);
        buffers.push(self.struct_buffer(
            ffi::VABufferType_VAEncSliceParameterBufferType,
            &self.slice_params(idr),
        )?);
        Ok(())
    }

    fn submit(&self, buffers: &mut [ffi::VABufferID]) -> Result<(), EncodeError> {
        let dpy = self.display.dpy;
        // SAFETY: valid context and input surface; buffers are live.
        unsafe {
            let st = ffi::vaBeginPicture(dpy, self.context, self.input);
            if st != OK {
                return Err(failed("vaBeginPicture", st));
            }
            let st = ffi::vaRenderPicture(
                dpy,
                self.context,
                buffers.as_mut_ptr(),
                i32::try_from(buffers.len()).unwrap_or(0),
            );
            let end = ffi::vaEndPicture(dpy, self.context);
            if st != OK {
                return Err(failed("vaRenderPicture", st));
            }
            if end != OK {
                return Err(failed("vaEndPicture", end));
            }
            let st = ffi::vaSyncSurface(dpy, self.input);
            if st != OK {
                return Err(failed("vaSyncSurface", st));
            }
        }
        Ok(())
    }

    /// Submits one picture and waits for it; returns the bitstream.
    fn encode_picture(&mut self, idr: bool) -> Result<Vec<u8>, EncodeError> {
        let recon = self.recon[self.next_recon];
        let mut buffers = Vec::with_capacity(10);
        let result = self
            .parameter_buffers(idr, recon, &mut buffers)
            .and_then(|()| self.submit(&mut buffers))
            .and_then(|()| self.read_coded());
        for b in buffers {
            // SAFETY: each parameter buffer destroyed once, after use.
            unsafe { ffi::vaDestroyBuffer(self.display.dpy, b) };
        }
        let data = result?;
        self.bitrate_changed = false;
        self.reference = Some(RefPic {
            surface: recon,
            frame_num: self.frame_num,
        });
        self.next_recon ^= 1;
        Ok(data)
    }

    fn read_coded(&self) -> Result<Vec<u8>, EncodeError> {
        let dpy = self.display.dpy;
        let mut seg: *mut c_void = ptr::null_mut();
        // SAFETY: the coded buffer is complete after vaSyncSurface.
        let st = unsafe { ffi::vaMapBuffer(dpy, self.coded, &raw mut seg) };
        if st != OK {
            return Err(failed("vaMapBuffer(coded)", st));
        }
        let mut out = Vec::new();
        let mut cur = seg.cast::<ffi::VACodedBufferSegment>();
        while !cur.is_null() {
            // SAFETY: libva's segment list, valid while mapped.
            unsafe {
                let s = &*cur;
                if !s.buf.is_null() {
                    out.extend_from_slice(std::slice::from_raw_parts(
                        s.buf.cast::<u8>(),
                        s.size as usize,
                    ));
                }
                cur = s.next.cast();
            }
        }
        // SAFETY: unmapping what was mapped above.
        unsafe { ffi::vaUnmapBuffer(dpy, self.coded) };
        Ok(out)
    }
}

/// Views a plain C struct as bytes (for libva parameter buffers).
fn as_bytes<T: Copy>(v: &T) -> &[u8] {
    // SAFETY: `T` is a `Copy` bindgen struct without pointers to Rust data;
    // reading its bytes (padding included) is what libva does too.
    unsafe { std::slice::from_raw_parts(ptr::from_ref(v).cast::<u8>(), size_of::<T>()) }
}

impl Drop for VaapiEncoder {
    fn drop(&mut self) {
        let dpy = self.display.dpy;
        // SAFETY: handles created by this encoder, each destroyed once,
        // before the display terminates; invalid ones are skipped.
        unsafe {
            if self.coded != ffi::VA_INVALID_ID {
                ffi::vaDestroyBuffer(dpy, self.coded);
            }
            if self.context != ffi::VA_INVALID_ID {
                ffi::vaDestroyContext(dpy, self.context);
            }
            if self.input != ffi::VA_INVALID_SURFACE {
                let mut surfaces = [self.input, self.recon[0], self.recon[1]];
                ffi::vaDestroySurfaces(dpy, surfaces.as_mut_ptr(), 3);
            }
            if self.va_config != ffi::VA_INVALID_ID {
                ffi::vaDestroyConfig(dpy, self.va_config);
            }
        }
    }
}

impl VideoEncoder for VaapiEncoder {
    fn name(&self) -> &'static str {
        "vaapi"
    }

    fn config(&self) -> EncoderConfig {
        self.config
    }

    fn encode(&mut self, frame: &Frame, force_keyframe: bool) -> Result<EncodedFrame, EncodeError> {
        let FrameData::Cpu(bgra) = &frame.data else {
            return Err(EncodeError::UnsupportedFrame(
                "VA-API needs CPU (BGRA) frames".into(),
            ));
        };
        if frame.width != self.config.width || frame.height != self.config.height {
            return Err(EncodeError::UnsupportedFrame(format!(
                "frame {}x{} doesn't match encoder {}x{}",
                frame.width, frame.height, self.config.width, self.config.height
            )));
        }
        self.upload(bgra)?;
        let idr = force_keyframe || self.reference.is_none();
        if idr {
            self.frame_num = 0;
            self.idr_id = self.idr_id.wrapping_add(1);
        }
        let mut data = match self.encode_picture(idr) {
            Ok(d) => d,
            Err(e) => {
                // Start clean: the next frame is a keyframe.
                self.reference = None;
                return Err(e);
            }
        };
        if idr && !h264::nal_types(&data).contains(&7) {
            // The driver wrote no parameter sets: supply ours.
            let mut with = h264::sps(&self.seq);
            with.extend(h264::pps(self.seq.profile));
            with.extend(data);
            data = with;
        }
        self.frame_num = (self.frame_num + 1) % (1 << (h264::LOG2_MAX_FRAME_NUM_MINUS4 + 4));
        Ok(EncodedFrame {
            kind: if idr {
                FrameKind::Key
            } else {
                FrameKind::Delta
            },
            capture_t_us: frame.capture_t_us,
            data,
        })
    }

    fn set_bitrate(&mut self, kbps: u32) {
        if kbps != self.config.bitrate_kbps {
            self.config.bitrate_kbps = kbps;
            self.bitrate_changed = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Needs a VA-API H.264 encoder and access to /dev/dri/renderD*:
    /// `cargo test -p ib-encode --features vaapi -- --ignored`.
    #[test]
    #[ignore = "needs VA-API hardware"]
    fn encodes_keyframe_then_deltas() {
        let config = EncoderConfig {
            codec: VideoCodec::H264,
            width: 640,
            height: 360,
            fps: 60,
            bitrate_kbps: 4000,
        };
        let mut enc = VaapiEncoder::new(config).expect("VA-API encoder");
        let mut kinds = Vec::new();
        for i in 0..10u8 {
            let px = [i * 20, 100, 200, 255].repeat(640 * 360);
            let f = Frame {
                capture_t_us: u64::from(i),
                width: 640,
                height: 360,
                data: FrameData::Cpu(px),
            };
            let out = enc.encode(&f, i == 6).expect("encode");
            assert_ne!(out.data, Vec::<u8>::new());
            if i == 0 {
                let types = h264::nal_types(&out.data);
                assert!(
                    types.contains(&7) && types.contains(&8) && types.contains(&5),
                    "{types:?}"
                );
            }
            kinds.push(out.kind);
        }
        assert_eq!(kinds[0], FrameKind::Key);
        assert_eq!(kinds[6], FrameKind::Key);
        assert_eq!(kinds[1], FrameKind::Delta);
    }
}
