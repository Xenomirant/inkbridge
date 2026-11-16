//! Hardware H.264 encoding with Media Foundation (Windows), zero-copy.
//!
//! Pipeline, entirely on the GPU that captured the frame:
//! 1. D3D11 video processor: BGRA → NV12, scaling and BT.709 colour in one
//!    pass (input: the capture texture; output: our NV12 texture);
//! 2. the vendor's hardware H.264 MFT (NVENC / Quick Sync / AMF, whichever
//!    drives that adapter), selected by adapter LUID, in **low-latency mode**:
//!    no B-frames, CBR, keyframes only on demand.
//!
//! Hardware MFTs are asynchronous: they signal `METransformNeedInput` and
//! `METransformHaveOutput` events. [`MfEncoder::encode`] feeds one frame and
//! waits (briefly) for its output, so each call returns the frame it was
//! given — the latency contract of [`VideoEncoder`].

use std::collections::HashMap;
use std::time::{Duration, Instant};

use ib_capture::{Frame, FrameData};
use ib_protocol::{FrameKind, VideoCodec};
use windows::Win32::Foundation::{RECT, VARIANT_TRUE};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_COLOR_SPACE,
    D3D11_VIDEO_PROCESSOR_CONTENT_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_STREAM,
    D3D11_VIDEO_USAGE_OPTIMAL_SPEED, D3D11_VPIV_DIMENSION_TEXTURE2D,
    D3D11_VPOV_DIMENSION_TEXTURE2D, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
    ID3D11VideoContext, ID3D11VideoDevice, ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator,
    ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Media::MediaFoundation::{
    CODECAPI_AVEncCommonMeanBitRate, CODECAPI_AVEncCommonRateControlMode,
    CODECAPI_AVEncMPVDefaultBPictureCount, CODECAPI_AVEncMPVGOPSize,
    CODECAPI_AVEncVideoForceKeyFrame, CODECAPI_AVLowLatencyMode, ICodecAPI, IMFActivate,
    IMFAttributes, IMFDXGIDeviceManager, IMFMediaEventGenerator, IMFSample, IMFTransform,
    METransformHaveOutput, METransformNeedInput, MF_E_NO_EVENTS_AVAILABLE,
    MF_E_TRANSFORM_STREAM_CHANGE, MF_EVENT_FLAG_NO_WAIT, MF_LOW_LATENCY, MF_MT_AVG_BITRATE,
    MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE,
    MF_MT_MPEG_SEQUENCE_HEADER, MF_MT_MPEG2_PROFILE, MF_MT_SUBTYPE, MF_MT_YUV_MATRIX,
    MF_TRANSFORM_ASYNC_UNLOCK, MF_VERSION, MFCreateAttributes, MFCreateDXGIDeviceManager,
    MFCreateDXGISurfaceBuffer, MFCreateMediaType, MFCreateSample, MFMediaType_Video,
    MFSTARTUP_FULL, MFSampleExtension_CleanPoint, MFStartup, MFT_CATEGORY_VIDEO_ENCODER,
    MFT_ENUM_ADAPTER_LUID, MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER,
    MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_START_OF_STREAM,
    MFT_MESSAGE_SET_D3D_MANAGER, MFT_OUTPUT_DATA_BUFFER, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES,
    MFT_REGISTER_TYPE_INFO, MFTEnum2, MFVideoFormat_H264, MFVideoFormat_NV12,
    MFVideoInterlace_Progressive, MFVideoTransferMatrix_BT709, eAVEncCommonRateControlMode_CBR,
    eAVEncH264VProfile_Main,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree};
use windows::Win32::System::Variant::{VARIANT, VT_BOOL, VT_UI4};
use windows::core::{GUID, Interface};

use crate::{EncodeError, EncodedFrame, EncoderConfig, VideoEncoder};

/// How long `encode` waits for the hardware to return the frame it was fed.
const OUTPUT_WAIT: Duration = Duration::from_millis(50);

fn err(context: &str) -> impl Fn(windows::core::Error) -> EncodeError + '_ {
    move |e| EncodeError::Unavailable(format!("{context}: {e}"))
}

pub struct MfEncoder {
    config: EncoderConfig,
    device: ID3D11Device,
    transform: IMFTransform,
    events: IMFMediaEventGenerator,
    codec_api: Option<ICodecAPI>,
    _manager: IMFDXGIDeviceManager,
    name: String,
    // GPU conversion.
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    vp_enum: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    nv12: ID3D11Texture2D,
    output_view: ID3D11VideoProcessorOutputView,
    input_views: HashMap<usize, ID3D11VideoProcessorInputView>,
    source_size: (u32, u32),
    // Async MFT state.
    need_input: u32,
    provides_samples: bool,
    sequence_header: Vec<u8>,
    frame_index: i64,
}

impl std::fmt::Debug for MfEncoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MfEncoder")
            .field("encoder", &self.name)
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

// SAFETY: the encoder is created and used on one thread (the video
// thread); the D3D11 device is multithread-protected by the capture backend.
unsafe impl Send for MfEncoder {}

impl MfEncoder {
    /// Creates a hardware encoder on `device` for `source` (capture size) →
    /// `config` (encoded size).
    pub fn new(
        device: &ID3D11Device,
        source: (u32, u32),
        config: EncoderConfig,
    ) -> Result<Self, EncodeError> {
        if config.codec != VideoCodec::H264 {
            return Err(EncodeError::Unavailable(
                "only H.264 is implemented for Media Foundation".into(),
            ));
        }
        // SAFETY: COM/MF initialisation is idempotent per thread/process;
        // every other call below is a COM method on a live interface with
        // valid arguments. Results are checked.
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            MFStartup(MF_VERSION, MFSTARTUP_FULL).map_err(err("MFStartup"))?;

            let (transform, name) = find_hardware_encoder(device)?;
            let attrs = transform
                .GetAttributes()
                .map_err(err("encoder attributes"))?;
            attrs
                .SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)
                .map_err(err("unlock async MFT"))?;
            let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);

            let mut token = 0u32;
            let mut manager = None;
            MFCreateDXGIDeviceManager(&mut token, &mut manager)
                .map_err(err("DXGI device manager"))?;
            let manager =
                manager.ok_or(EncodeError::Unavailable("no DXGI device manager".into()))?;
            manager
                .ResetDevice(device, token)
                .map_err(err("ResetDevice"))?;
            transform
                .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)
                .map_err(err("encoder rejected our GPU device"))?;

            set_output_type(&transform, &config)?;
            set_input_type(&transform, &config)?;
            let codec_api = transform.cast::<ICodecAPI>().ok();
            if let Some(api) = &codec_api {
                configure_low_latency(api, &config);
            }

            let info = transform
                .GetOutputStreamInfo(0)
                .map_err(err("output stream info"))?;
            let provides_samples = info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;
            let sequence_header = transform
                .GetOutputCurrentType(0)
                .ok()
                .and_then(|t| blob(&t, &MF_MT_MPEG_SEQUENCE_HEADER))
                .unwrap_or_default();

            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .map_err(err("begin streaming"))?;
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .map_err(err("start of stream"))?;
            let events = transform
                .cast::<IMFMediaEventGenerator>()
                .map_err(err("encoder events"))?;

            let (video_device, video_context, vp_enum, processor, nv12, output_view) =
                create_converter(device, source, (config.width, config.height))?;

            Ok(Self {
                config,
                device: device.clone(),
                transform,
                events,
                codec_api,
                _manager: manager,
                name,
                video_device,
                video_context,
                vp_enum,
                processor,
                nv12,
                output_view,
                input_views: HashMap::new(),
                source_size: source,
                need_input: 0,
                provides_samples,
                sequence_header,
                frame_index: 0,
            })
        }
    }

    pub fn encoder_name(&self) -> &str {
        &self.name
    }

    /// Converts the captured BGRA texture into our NV12 texture on the GPU.
    fn convert(&mut self, source: &ID3D11Texture2D) -> Result<(), EncodeError> {
        let key = source.as_raw() as usize;
        if !self.input_views.contains_key(&key) {
            if self.input_views.len() > 8 {
                self.input_views.clear(); // capture pool was recreated
            }
            let desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPIV {
                        MipSlice: 0,
                        ArraySlice: 0,
                    },
                },
            };
            let mut view = None;
            // SAFETY: valid texture/enumerator from this device, valid descriptor.
            unsafe {
                self.video_device.CreateVideoProcessorInputView(
                    source,
                    &self.vp_enum,
                    &desc,
                    Some(&mut view),
                )
            }
            .map_err(|e| EncodeError::Failed(format!("input view: {e}")))?;
            self.input_views.insert(
                key,
                view.ok_or(EncodeError::Failed("no input view".into()))?,
            );
        }
        let stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            pInputSurface: std::mem::ManuallyDrop::new(self.input_views.get(&key).cloned()),
            ..Default::default()
        };
        let streams = [stream];
        // SAFETY: processor, view and stream all belong to this device.
        let r = unsafe {
            self.video_context
                .VideoProcessorBlt(&self.processor, &self.output_view, 0, &streams)
        };
        // Release the extra reference held by the ManuallyDrop field.
        let [mut s] = streams;
        // SAFETY: we own this clone and drop it exactly once.
        unsafe { std::mem::ManuallyDrop::drop(&mut s.pInputSurface) };
        r.map_err(|e| EncodeError::Failed(format!("colour conversion: {e}")))
    }

    /// Handles pending encoder events without blocking; returns output if any.
    fn pump(&mut self) -> Result<Option<(Vec<u8>, bool)>, EncodeError> {
        let mut out = None;
        loop {
            // SAFETY: valid event generator.
            let event = match unsafe { self.events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(e) => e,
                Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => return Ok(out),
                Err(e) => return Err(EncodeError::Failed(format!("encoder event: {e}"))),
            };
            // SAFETY: valid event.
            let kind = unsafe { event.GetType() }.unwrap_or(0);
            if kind == METransformNeedInput.0 as u32 {
                self.need_input += 1;
            } else if kind == METransformHaveOutput.0 as u32 {
                out = self.take_output()?.or(out);
            }
        }
    }

    fn take_output(&mut self) -> Result<Option<(Vec<u8>, bool)>, EncodeError> {
        let mut buffer = MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            ..Default::default()
        };
        if !self.provides_samples {
            // SAFETY: plain MF factory calls.
            unsafe {
                let sample = MFCreateSample().map_err(|e| EncodeError::Failed(e.to_string()))?;
                let size = self
                    .transform
                    .GetOutputStreamInfo(0)
                    .map_or(8 << 20, |i| i.cbSize)
                    .max(1 << 20);
                let mem = windows::Win32::Media::MediaFoundation::MFCreateMemoryBuffer(size)
                    .map_err(|e| EncodeError::Failed(e.to_string()))?;
                sample
                    .AddBuffer(&mem)
                    .map_err(|e| EncodeError::Failed(e.to_string()))?;
                buffer.pSample = std::mem::ManuallyDrop::new(Some(sample));
            }
        }
        let mut status = 0u32;
        // SAFETY: one output buffer descriptor, valid for the call.
        let r = unsafe {
            self.transform
                .ProcessOutput(0, std::slice::from_mut(&mut buffer), &mut status)
        };
        // SAFETY: we take ownership of what ProcessOutput left in the struct.
        let sample = unsafe { std::mem::ManuallyDrop::take(&mut buffer.pSample) };
        let _events = unsafe { std::mem::ManuallyDrop::take(&mut buffer.pEvents) };
        match r {
            Ok(()) => {}
            Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                // The encoder wants its output type re-confirmed.
                // SAFETY: valid transform.
                unsafe {
                    if let Ok(t) = self.transform.GetOutputAvailableType(0, 0) {
                        let _ = self.transform.SetOutputType(0, &t, 0);
                    }
                }
                return Ok(None);
            }
            Err(e) => return Err(EncodeError::Failed(format!("ProcessOutput: {e}"))),
        }
        let Some(sample) = sample else {
            return Ok(None);
        };
        Ok(Some(read_sample(&sample)?))
    }
}

impl VideoEncoder for MfEncoder {
    fn name(&self) -> &'static str {
        "media-foundation"
    }

    fn config(&self) -> EncoderConfig {
        self.config
    }

    fn encode(&mut self, frame: &Frame, force_keyframe: bool) -> Result<EncodedFrame, EncodeError> {
        let FrameData::Gpu(gpu) = &frame.data else {
            return Err(EncodeError::UnsupportedFrame(
                "Media Foundation path needs GPU frames".into(),
            ));
        };
        if gpu.device != self.device || (frame.width, frame.height) != self.source_size {
            return Err(EncodeError::UnsupportedFrame(
                "frame from another device or size".into(),
            ));
        }
        self.convert(&gpu.texture)?;

        // Wait for the encoder to ask for input (it normally already has).
        let deadline = Instant::now() + OUTPUT_WAIT;
        let mut early = self.pump()?;
        while self.need_input == 0 {
            if Instant::now() > deadline {
                return Err(EncodeError::Failed(
                    "hardware encoder stopped asking for input".into(),
                ));
            }
            std::thread::sleep(Duration::from_micros(200));
            early = self.pump()?.or(early);
        }

        if force_keyframe && let Some(api) = &self.codec_api {
            // SAFETY: valid ICodecAPI, VARIANT built for VT_UI4.
            let _ = unsafe { api.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &variant_u32(1)) };
        }
        // SAFETY: MF sample wrapping our NV12 texture (same device).
        unsafe {
            let buffer = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, &self.nv12, 0, false)
                .map_err(|e| EncodeError::Failed(format!("surface buffer: {e}")))?;
            let sample = MFCreateSample().map_err(|e| EncodeError::Failed(e.to_string()))?;
            sample
                .AddBuffer(&buffer)
                .map_err(|e| EncodeError::Failed(e.to_string()))?;
            let frame_100ns = 10_000_000 / i64::from(self.config.fps.max(1));
            sample
                .SetSampleTime(self.frame_index * frame_100ns)
                .map_err(|e| EncodeError::Failed(e.to_string()))?;
            sample
                .SetSampleDuration(frame_100ns)
                .map_err(|e| EncodeError::Failed(e.to_string()))?;
            self.transform
                .ProcessInput(0, &sample, 0)
                .map_err(|e| EncodeError::Failed(format!("ProcessInput: {e}")))?;
        }
        self.need_input -= 1;
        self.frame_index += 1;

        // Wait for this frame's output: low-latency mode emits one per input.
        let mut output = early;
        while output.is_none() {
            if Instant::now() > deadline {
                return Err(EncodeError::Failed(
                    "hardware encoder produced no output in time".into(),
                ));
            }
            output = self.pump()?;
            if output.is_none() {
                std::thread::sleep(Duration::from_micros(200));
            }
        }
        let (mut data, key) = output.expect("checked above");
        // Some encoders keep SPS/PPS only in the media type: prepend them
        // to keyframes so the decoder can always start there.
        if key && !has_nal(&data, 7) && !self.sequence_header.is_empty() {
            let mut with_header = self.sequence_header.clone();
            with_header.extend_from_slice(&data);
            data = with_header;
        }
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
        if let Some(api) = &self.codec_api {
            // SAFETY: valid ICodecAPI, VARIANT built for VT_UI4.
            let _ = unsafe {
                api.SetValue(
                    &CODECAPI_AVEncCommonMeanBitRate,
                    &variant_u32(kbps.saturating_mul(1000)),
                )
            };
        }
    }
}

/// The first hardware H.264 encoder on the adapter that owns `device`.
unsafe fn find_hardware_encoder(
    device: &ID3D11Device,
) -> Result<(IMFTransform, String), EncodeError> {
    // SAFETY (whole fn): COM calls with valid arguments; the activation array
    // is released and freed exactly once below.
    unsafe {
        let dxgi = device.cast::<IDXGIDevice>().map_err(err("DXGI device"))?;
        let luid = dxgi
            .GetAdapter()
            .and_then(|a| a.GetDesc())
            .map_err(err("adapter"))?
            .AdapterLuid;
        let mut attrs: Option<IMFAttributes> = None;
        MFCreateAttributes(&mut attrs, 1).map_err(err("attributes"))?;
        let attrs = attrs.ok_or(EncodeError::Unavailable("no attributes".into()))?;
        let luid64 = (u64::from(luid.HighPart.cast_unsigned()) << 32) | u64::from(luid.LowPart);
        attrs
            .SetUINT64(&MFT_ENUM_ADAPTER_LUID, luid64)
            .map_err(err("adapter filter"))?;

        let input = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_NV12,
        };
        let output = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_H264,
        };
        let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count = 0u32;
        MFTEnum2(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
            Some(&input),
            Some(&output),
            &attrs,
            &mut list,
            &mut count,
        )
        .map_err(err("enumerating hardware encoders"))?;
        let activations = if list.is_null() {
            &mut [][..]
        } else {
            std::slice::from_raw_parts_mut(list, count as usize)
        };
        let first = activations.first().cloned().flatten();
        for a in activations.iter_mut() {
            drop(a.take());
        }
        if !list.is_null() {
            CoTaskMemFree(Some(list.cast()));
        }
        let activate = first.ok_or(EncodeError::Unavailable(
            "no hardware H.264 encoder on this GPU".into(),
        ))?;
        let name = friendly_name(&activate).unwrap_or_else(|| "hardware H.264 encoder".into());
        let transform = activate
            .ActivateObject::<IMFTransform>()
            .map_err(err("activating encoder"))?;
        Ok((transform, name))
    }
}

fn friendly_name(activate: &IMFActivate) -> Option<String> {
    use windows::Win32::Media::MediaFoundation::MFT_FRIENDLY_NAME_Attribute;
    let mut buf = [0u16; 256];
    let mut len = 0u32;
    // SAFETY: buffer and length pointers are valid for the call.
    unsafe { activate.GetString(&MFT_FRIENDLY_NAME_Attribute, &mut buf, Some(&mut len)) }.ok()?;
    Some(String::from_utf16_lossy(&buf[..len as usize]))
}

unsafe fn set_output_type(t: &IMFTransform, c: &EncoderConfig) -> Result<(), EncodeError> {
    // SAFETY: media type attribute setters with valid GUID pointers.
    unsafe {
        let mt = MFCreateMediaType().map_err(err("media type"))?;
        mt.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .map_err(err("major type"))?;
        mt.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)
            .map_err(err("subtype"))?;
        mt.SetUINT32(&MF_MT_AVG_BITRATE, c.bitrate_kbps.saturating_mul(1000))
            .map_err(err("bitrate"))?;
        mt.SetUINT64(&MF_MT_FRAME_SIZE, pack(c.width, c.height))
            .map_err(err("frame size"))?;
        mt.SetUINT64(&MF_MT_FRAME_RATE, pack(u32::from(c.fps), 1))
            .map_err(err("frame rate"))?;
        mt.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
            .map_err(err("interlace"))?;
        mt.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Main.0 as u32)
            .map_err(err("profile"))?;
        let _ = mt.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32);
        t.SetOutputType(0, &mt, 0)
            .map_err(err("encoder output type"))
    }
}

unsafe fn set_input_type(t: &IMFTransform, c: &EncoderConfig) -> Result<(), EncodeError> {
    // SAFETY: as above.
    unsafe {
        let mt = MFCreateMediaType().map_err(err("media type"))?;
        mt.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .map_err(err("major type"))?;
        mt.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)
            .map_err(err("subtype"))?;
        mt.SetUINT64(&MF_MT_FRAME_SIZE, pack(c.width, c.height))
            .map_err(err("frame size"))?;
        mt.SetUINT64(&MF_MT_FRAME_RATE, pack(u32::from(c.fps), 1))
            .map_err(err("frame rate"))?;
        mt.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
            .map_err(err("interlace"))?;
        t.SetInputType(0, &mt, 0).map_err(err("encoder input type"))
    }
}

/// Latency over everything. Unsupported properties are skipped: encoders
/// differ, and none of these is essential for correctness.
fn configure_low_latency(api: &ICodecAPI, c: &EncoderConfig) {
    let settings: [(GUID, VARIANT); 5] = [
        (CODECAPI_AVLowLatencyMode, variant_bool(true)),
        (
            CODECAPI_AVEncCommonRateControlMode,
            variant_u32(eAVEncCommonRateControlMode_CBR.0 as u32),
        ),
        (
            CODECAPI_AVEncCommonMeanBitRate,
            variant_u32(c.bitrate_kbps.saturating_mul(1000)),
        ),
        (CODECAPI_AVEncMPVDefaultBPictureCount, variant_u32(0)),
        // Very long GOP: keyframes are requested explicitly when needed.
        (CODECAPI_AVEncMPVGOPSize, variant_u32(18_000)),
    ];
    for (key, value) in &settings {
        // SAFETY: valid ICodecAPI and VARIANT.
        if let Err(e) = unsafe { api.SetValue(key, value) } {
            tracing_like_debug(key, &e);
        }
    }
}

#[allow(clippy::type_complexity)]
fn create_converter(
    device: &ID3D11Device,
    source: (u32, u32),
    target: (u32, u32),
) -> Result<
    (
        ID3D11VideoDevice,
        ID3D11VideoContext,
        ID3D11VideoProcessorEnumerator,
        ID3D11VideoProcessor,
        ID3D11Texture2D,
        ID3D11VideoProcessorOutputView,
    ),
    EncodeError,
> {
    // SAFETY: D3D11 video APIs on a live device with valid descriptors.
    unsafe {
        let video_device = device
            .cast::<ID3D11VideoDevice>()
            .map_err(err("video device"))?;
        let context: ID3D11DeviceContext = device
            .GetImmediateContext()
            .map_err(err("device context"))?;
        let video_context = context
            .cast::<ID3D11VideoContext>()
            .map_err(err("video context"))?;
        let rate = DXGI_RATIONAL {
            Numerator: 60,
            Denominator: 1,
        };
        let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: rate,
            InputWidth: source.0,
            InputHeight: source.1,
            OutputFrameRate: rate,
            OutputWidth: target.0,
            OutputHeight: target.1,
            Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
        };
        let vp_enum = video_device
            .CreateVideoProcessorEnumerator(&content)
            .map_err(err("video processor"))?;
        let processor = video_device
            .CreateVideoProcessor(&vp_enum, 0)
            .map_err(err("video processor"))?;

        // Input: full-range RGB. Output: limited-range BT.709 YCbCr, matching
        // what the encoder signals, so colours are right on the tablet.
        let input_cs = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0 };
        let output_cs = D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
            _bitfield: (1 << 2) | (1 << 4),
        };
        video_context.VideoProcessorSetStreamColorSpace(&processor, 0, &input_cs);
        video_context.VideoProcessorSetOutputColorSpace(&processor, &output_cs);
        video_context.VideoProcessorSetStreamFrameFormat(
            &processor,
            0,
            D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
        );
        let full = RECT {
            left: 0,
            top: 0,
            right: i32::try_from(source.0).unwrap_or(i32::MAX),
            bottom: i32::try_from(source.1).unwrap_or(i32::MAX),
        };
        video_context.VideoProcessorSetStreamSourceRect(&processor, 0, true, Some(&full));

        let desc = D3D11_TEXTURE2D_DESC {
            Width: target.0,
            Height: target.1,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut nv12 = None;
        device
            .CreateTexture2D(&desc, None, Some(&mut nv12))
            .map_err(err("NV12 texture"))?;
        let nv12 = nv12.ok_or(EncodeError::Unavailable("no NV12 texture".into()))?;
        let view_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
            },
        };
        let mut output_view = None;
        video_device
            .CreateVideoProcessorOutputView(&nv12, &vp_enum, &view_desc, Some(&mut output_view))
            .map_err(err("NV12 output view"))?;
        let output_view = output_view.ok_or(EncodeError::Unavailable("no output view".into()))?;
        Ok((
            video_device,
            video_context,
            vp_enum,
            processor,
            nv12,
            output_view,
        ))
    }
}

fn read_sample(sample: &IMFSample) -> Result<(Vec<u8>, bool), EncodeError> {
    // SAFETY: lock/unlock a contiguous buffer; the slice is copied before unlocking.
    unsafe {
        let buffer = sample
            .ConvertToContiguousBuffer()
            .map_err(|e| EncodeError::Failed(e.to_string()))?;
        let mut ptr = std::ptr::null_mut();
        let mut len = 0u32;
        buffer
            .Lock(&mut ptr, None, Some(&mut len))
            .map_err(|e| EncodeError::Failed(e.to_string()))?;
        let data = std::slice::from_raw_parts(ptr, len as usize).to_vec();
        let _ = buffer.Unlock();
        let key =
            sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0 || has_nal(&data, 5);
        Ok((data, key))
    }
}

fn blob(t: &windows::Win32::Media::MediaFoundation::IMFMediaType, key: &GUID) -> Option<Vec<u8>> {
    // SAFETY: size query then a read into a buffer of that size.
    unsafe {
        let size = t.GetBlobSize(key).ok()? as usize;
        let mut buf = vec![0u8; size];
        let mut got = 0u32;
        t.GetBlob(key, &mut buf, Some(&mut got)).ok()?;
        buf.truncate(got as usize);
        Some(buf)
    }
}

/// Whether an Annex-B stream contains a NAL unit of `nal_type`.
fn has_nal(data: &[u8], nal_type: u8) -> bool {
    data.windows(4).enumerate().any(|(i, w)| {
        let header = if w[..3] == [0, 0, 1] {
            data.get(i + 3)
        } else if w == [0, 0, 0, 1] {
            data.get(i + 4)
        } else {
            None
        };
        header.is_some_and(|h| h & 0x1F == nal_type)
    })
}

const fn pack(hi: u32, lo: u32) -> u64 {
    ((hi as u64) << 32) | lo as u64
}

fn variant_u32(v: u32) -> VARIANT {
    let mut var = VARIANT::default();
    // SAFETY: writing the active union member that matches the tag we set.
    unsafe {
        let inner = &mut *var.Anonymous.Anonymous;
        inner.vt = VT_UI4;
        inner.Anonymous.ulVal = v;
    }
    var
}

fn variant_bool(v: bool) -> VARIANT {
    let mut var = VARIANT::default();
    // SAFETY: as above, for VT_BOOL.
    unsafe {
        let inner = &mut *var.Anonymous.Anonymous;
        inner.vt = VT_BOOL;
        inner.Anonymous.boolVal = if v {
            VARIANT_TRUE
        } else {
            windows::Win32::Foundation::VARIANT_FALSE
        };
    }
    var
}

/// `ib-encode` has no logging dependency; unsupported codec properties are
/// expected on some encoders and are only interesting when debugging.
fn tracing_like_debug(key: &GUID, e: &windows::core::Error) {
    if std::env::var_os("INKBRIDGE_DEBUG_ENCODER").is_some() {
        eprintln!("encoder property {key:?} not applied: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_nal_types_with_both_start_codes() {
        let au = [
            0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 9,
        ];
        assert!(has_nal(&au, 7) && has_nal(&au, 8) && has_nal(&au, 5));
        assert!(!has_nal(&au, 1));
    }
}
