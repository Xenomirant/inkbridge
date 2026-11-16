//! Minimal H.264 parameter-set writer for hardware encoders that take
//! "packed" headers from the application (VA-API), or that leave them out.
//!
//! Only the subset Inkbridge uses: progressive 4:2:0 8-bit, one reference
//! frame, no B-frames, `pic_order_cnt_type` 2 (display order = decode
//! order), BT.709 limited range, and a VUI that tells decoders no frame is
//! ever reordered, so they can show each frame as soon as it's decoded.

/// The H.264 profiles we encode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    ConstrainedBaseline,
    Main,
    High,
}

impl Profile {
    pub fn idc(self) -> u8 {
        match self {
            Self::ConstrainedBaseline => 66,
            Self::Main => 77,
            Self::High => 100,
        }
    }

    /// CABAC (better compression) is available from Main up.
    pub fn cabac(self) -> bool {
        self != Self::ConstrainedBaseline
    }

    /// 8x8 transforms (High only).
    pub fn transform_8x8(self) -> bool {
        self == Self::High
    }
}

/// `log2_max_frame_num_minus4`: frame numbers wrap at 256.
pub const LOG2_MAX_FRAME_NUM_MINUS4: u32 = 4;

/// Encoded-picture geometry and level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SeqParams {
    pub profile: Profile,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

impl SeqParams {
    pub fn width_mbs(&self) -> u32 {
        self.width.div_ceil(16)
    }

    pub fn height_mbs(&self) -> u32 {
        self.height.div_ceil(16)
    }

    /// Right/bottom cropping in chroma units (2 pixels for 4:2:0).
    pub fn crop(&self) -> (u32, u32) {
        (
            (self.width_mbs() * 16 - self.width) / 2,
            (self.height_mbs() * 16 - self.height) / 2,
        )
    }

    /// Lowest level (×10) whose frame-size and macroblock-rate limits fit.
    pub fn level_idc(&self) -> u8 {
        // (level_idc, max macroblocks per second, max frame size in MBs)
        const LEVELS: [(u8, u64, u64); 8] = [
            (31, 108_000, 3_600),
            (32, 216_000, 5_120),
            (40, 245_760, 8_192),
            (42, 522_240, 8_704),
            (50, 589_824, 22_080),
            (51, 983_040, 36_864),
            (52, 2_073_600, 36_864),
            (60, 4_177_920, 139_264),
        ];
        let mbs = u64::from(self.width_mbs()) * u64::from(self.height_mbs());
        let rate = mbs * u64::from(self.fps.max(1));
        LEVELS
            .iter()
            .find(|(_, max_rate, max_size)| mbs <= *max_size && rate <= *max_rate)
            .map_or(60, |l| l.0)
    }
}

/// Writes bits MSB-first, with Exp-Golomb codes.
#[derive(Debug, Default)]
pub struct BitWriter {
    bytes: Vec<u8>,
    cur: u8,
    n: u8,
}

impl BitWriter {
    pub fn bit(&mut self, b: bool) {
        self.cur = (self.cur << 1) | u8::from(b);
        self.n += 1;
        if self.n == 8 {
            self.bytes.push(self.cur);
            self.cur = 0;
            self.n = 0;
        }
    }

    /// The low `n` bits of `v`, most significant first.
    pub fn bits(&mut self, n: u32, v: u32) {
        for i in (0..n).rev() {
            self.bit((v >> i) & 1 == 1);
        }
    }

    /// Unsigned Exp-Golomb `ue(v)`.
    pub fn ue(&mut self, v: u32) {
        let x = u64::from(v) + 1;
        let len = 64 - x.leading_zeros();
        self.bits(len - 1, 0);
        for i in (0..len).rev() {
            self.bit((x >> i) & 1 == 1);
        }
    }

    /// Signed Exp-Golomb `se(v)`.
    pub fn se(&mut self, v: i32) {
        let mapped = if v > 0 {
            v.unsigned_abs() * 2 - 1
        } else {
            v.unsigned_abs() * 2
        };
        self.ue(mapped);
    }

    /// `rbsp_trailing_bits`: a stop bit, then zeros to the byte boundary.
    pub fn trailing(mut self) -> Vec<u8> {
        self.bit(true);
        while self.n != 0 {
            self.bit(false);
        }
        self.bytes
    }

    /// Bits written so far (for packed-header lengths).
    pub fn len_bits(&self) -> usize {
        self.bytes.len() * 8 + usize::from(self.n)
    }
}

/// Wraps an RBSP into an Annex-B NAL unit (start code, header, emulation
/// prevention bytes).
pub fn nal(nal_ref_idc: u8, nal_type: u8, rbsp: &[u8]) -> Vec<u8> {
    let mut out = vec![0, 0, 0, 1, (nal_ref_idc << 5) | nal_type];
    let mut zeros = 0;
    for &b in rbsp {
        if zeros >= 2 && b <= 3 {
            out.push(3);
            zeros = 0;
        }
        out.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
    out
}

/// Sequence parameter set NAL (Annex-B).
pub fn sps(p: &SeqParams) -> Vec<u8> {
    let mut w = BitWriter::default();
    w.bits(8, u32::from(p.profile.idc()));
    // constraint_set0..5 flags + 2 reserved bits.
    let constraints: u32 = match p.profile {
        Profile::ConstrainedBaseline => 0b1100_0000,
        Profile::Main => 0b0100_0000,
        Profile::High => 0,
    };
    w.bits(8, constraints);
    w.bits(8, u32::from(p.level_idc()));
    w.ue(0); // seq_parameter_set_id
    if p.profile == Profile::High {
        w.ue(1); // chroma_format_idc 4:2:0
        w.ue(0); // bit_depth_luma_minus8
        w.ue(0); // bit_depth_chroma_minus8
        w.bit(false); // qpprime_y_zero_transform_bypass_flag
        w.bit(false); // seq_scaling_matrix_present_flag
    }
    w.ue(LOG2_MAX_FRAME_NUM_MINUS4);
    w.ue(2); // pic_order_cnt_type: POC follows frame_num
    w.ue(1); // max_num_ref_frames
    w.bit(false); // gaps_in_frame_num_value_allowed_flag
    w.ue(p.width_mbs() - 1);
    w.ue(p.height_mbs() - 1);
    w.bit(true); // frame_mbs_only_flag
    w.bit(true); // direct_8x8_inference_flag
    let (crop_r, crop_b) = p.crop();
    let cropping = crop_r != 0 || crop_b != 0;
    w.bit(cropping);
    if cropping {
        w.ue(0);
        w.ue(crop_r);
        w.ue(0);
        w.ue(crop_b);
    }
    w.bit(true); // vui_parameters_present_flag
    write_vui(&mut w);
    nal(3, 7, &w.trailing())
}

fn write_vui(w: &mut BitWriter) {
    w.bit(false); // aspect_ratio_info_present_flag (square pixels)
    w.bit(false); // overscan_info_present_flag
    w.bit(true); // video_signal_type_present_flag
    w.bits(3, 5); // video_format: unspecified
    w.bit(false); // video_full_range_flag: limited range
    w.bit(true); // colour_description_present_flag
    w.bits(8, 1); // colour_primaries: BT.709
    w.bits(8, 1); // transfer_characteristics: BT.709
    w.bits(8, 1); // matrix_coefficients: BT.709
    w.bit(false); // chroma_loc_info_present_flag
    w.bit(false); // timing_info_present_flag (variable frame rate)
    w.bit(false); // nal_hrd_parameters_present_flag
    w.bit(false); // vcl_hrd_parameters_present_flag
    w.bit(false); // pic_struct_present_flag
    w.bit(true); // bitstream_restriction_flag
    w.bit(true); // motion_vectors_over_pic_boundaries_flag
    w.ue(0); // max_bytes_per_pic_denom
    w.ue(0); // max_bits_per_mb_denom
    w.ue(16); // log2_max_mv_length_horizontal
    w.ue(16); // log2_max_mv_length_vertical
    w.ue(0); // max_num_reorder_frames: show every frame immediately
    w.ue(1); // max_dec_frame_buffering
}

/// Picture parameter set NAL (Annex-B).
pub fn pps(profile: Profile) -> Vec<u8> {
    let mut w = BitWriter::default();
    w.ue(0); // pic_parameter_set_id
    w.ue(0); // seq_parameter_set_id
    w.bit(profile.cabac()); // entropy_coding_mode_flag
    w.bit(false); // bottom_field_pic_order_in_frame_present_flag
    w.ue(0); // num_slice_groups_minus1
    w.ue(0); // num_ref_idx_l0_default_active_minus1
    w.ue(0); // num_ref_idx_l1_default_active_minus1
    w.bit(false); // weighted_pred_flag
    w.bits(2, 0); // weighted_bipred_idc
    w.se(0); // pic_init_qp_minus26
    w.se(0); // pic_init_qs_minus26
    w.se(0); // chroma_qp_index_offset
    w.bit(true); // deblocking_filter_control_present_flag
    w.bit(false); // constrained_intra_pred_flag
    w.bit(false); // redundant_pic_cnt_present_flag
    if profile.transform_8x8() {
        w.bit(true); // transform_8x8_mode_flag
        w.bit(false); // pic_scaling_matrix_present_flag
        w.se(0); // second_chroma_qp_index_offset
    }
    nal(3, 8, &w.trailing())
}

/// NAL unit types present in an Annex-B stream.
pub fn nal_types(stream: &[u8]) -> Vec<u8> {
    let mut types = Vec::new();
    let mut i = 0;
    while i + 3 < stream.len() {
        if stream[i] == 0 && stream[i + 1] == 0 && stream[i + 2] == 1 {
            types.push(stream[i + 3] & 0x1F);
            i += 3;
        } else {
            i += 1;
        }
    }
    types
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reads bits back for round-trip tests.
    struct BitReader<'a> {
        data: &'a [u8],
        pos: usize,
    }

    impl BitReader<'_> {
        fn bit(&mut self) -> u32 {
            let b = (self.data[self.pos / 8] >> (7 - self.pos % 8)) & 1;
            self.pos += 1;
            u32::from(b)
        }
        fn bits(&mut self, n: u32) -> u32 {
            (0..n).fold(0, |acc, _| (acc << 1) | self.bit())
        }
        fn ue(&mut self) -> u32 {
            let mut zeros = 0;
            while self.bit() == 0 {
                zeros += 1;
            }
            (1 << zeros) - 1 + self.bits(zeros)
        }
    }

    #[test]
    fn exp_golomb_matches_the_spec_table() {
        // ue: 0→1, 1→010, 2→011, 3→00100; se: 1→010, -1→011.
        let mut w = BitWriter::default();
        for v in [0, 1, 2, 3] {
            w.ue(v);
        }
        w.se(1);
        w.se(-1);
        assert_eq!(w.len_bits(), 1 + 3 + 3 + 5 + 3 + 3);
        let bytes = w.trailing();
        let mut r = BitReader {
            data: &bytes,
            pos: 0,
        };
        assert_eq!(
            [r.ue(), r.ue(), r.ue(), r.ue(), r.ue(), r.ue()],
            [0, 1, 2, 3, 1, 2]
        );
    }

    #[test]
    fn emulation_prevention_is_inserted() {
        let n = nal(3, 7, &[0, 0, 1, 0, 0, 0, 5]);
        assert_eq!(&n[5..], &[0, 0, 3, 1, 0, 0, 3, 0, 5]);
    }

    #[test]
    fn sps_encodes_size_and_cropping() {
        let p = SeqParams {
            profile: Profile::High,
            width: 1920,
            height: 1080,
            fps: 60,
        };
        assert_eq!(p.crop(), (0, 4));
        assert_eq!(p.level_idc(), 42);
        let s = sps(&p);
        assert_eq!(&s[..5], &[0, 0, 0, 1, 0x67]);
        let mut r = BitReader {
            data: &s[5..],
            pos: 0,
        };
        assert_eq!(r.bits(8), 100);
        r.bits(16);
        assert_eq!(r.ue(), 0); // sps id
        assert_eq!(r.ue(), 1); // chroma 4:2:0
        r.ue();
        r.ue();
        r.bits(2);
        assert_eq!(r.ue(), LOG2_MAX_FRAME_NUM_MINUS4);
        assert_eq!(r.ue(), 2);
        assert_eq!(r.ue(), 1);
        r.bit();
        assert_eq!(r.ue() + 1, 120); // width in MBs
        assert_eq!(r.ue() + 1, 68); // height in MBs
        assert_eq!(r.bits(2), 0b11);
        assert_eq!(r.bit(), 1); // cropping
        assert_eq!([r.ue(), r.ue(), r.ue(), r.ue()], [0, 0, 0, 4]);
    }

    #[test]
    fn levels_grow_with_resolution() {
        let lvl = |width, height, fps| {
            SeqParams {
                profile: Profile::Main,
                width,
                height,
                fps,
            }
            .level_idc()
        };
        assert_eq!(lvl(1280, 720, 30), 31);
        assert_eq!(lvl(1920, 1080, 30), 40);
        assert_eq!(lvl(2560, 1440, 60), 51);
    }

    #[test]
    fn pps_starts_with_header_and_finds_nal_types() {
        let mut stream = sps(&SeqParams {
            profile: Profile::Main,
            width: 640,
            height: 480,
            fps: 60,
        });
        stream.extend(pps(Profile::Main));
        assert_eq!(nal_types(&stream), [7, 8]);
    }
}
