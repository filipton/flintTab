//! H.264 SPS rewrite that tells the decoder it never has to hold frames back.
//!
//! VideoToolbox's SPS does not carry VUI `bitstream_restriction`, so a decoder has to assume
//! frames may be reordered and many Android decoders then keep several decoded frames in
//! their DPB before releasing the first one (tens of ms of added latency). Moonlight patches
//! the SPS the same way: `max_num_reorder_frames = 0`, `max_dec_frame_buffering = num_ref_frames`.
//! Everything before the restriction fields is copied bit for bit.

struct Reader<'a> {
    data: &'a [u8],
    pos: usize, // bit position
}

impl Reader<'_> {
    fn bit(&mut self) -> Option<u32> {
        let byte = *self.data.get(self.pos / 8)?;
        let b = (byte >> (7 - self.pos % 8)) & 1;
        self.pos += 1;
        Some(b as u32)
    }
    fn bits(&mut self, n: u32) -> Option<u32> {
        let mut v = 0;
        for _ in 0..n {
            v = (v << 1) | self.bit()?;
        }
        Some(v)
    }
    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        Some((1u32 << zeros) - 1 + self.bits(zeros)?)
    }
    fn se(&mut self) -> Option<i32> {
        let k = self.ue()?;
        Some(if k & 1 == 1 { k.div_ceil(2) as i32 } else { -((k / 2) as i32) })
    }
}

struct Writer {
    out: Vec<u8>,
    bits: usize,
}

impl Writer {
    fn bit(&mut self, b: u32) {
        if self.bits.is_multiple_of(8) {
            self.out.push(0);
        }
        if b != 0 {
            *self.out.last_mut().unwrap() |= 1 << (7 - self.bits % 8);
        }
        self.bits += 1;
    }
    fn bits(&mut self, v: u32, n: u32) {
        for i in (0..n).rev() {
            self.bit((v >> i) & 1);
        }
    }
    fn ue(&mut self, v: u32) {
        let x = v as u64 + 1;
        let len = 64 - x.leading_zeros();
        self.bits(0, len - 1);
        for i in (0..len).rev() {
            self.bit(((x >> i) & 1) as u32);
        }
    }
}

fn unescape(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for &b in nal {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

fn escape(rbsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rbsp.len() + 4);
    let mut zeros = 0;
    for &b in rbsp {
        if zeros >= 2 && b <= 3 {
            out.push(3);
            zeros = 0;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

fn skip_scaling_list(r: &mut Reader, size: usize) -> Option<()> {
    let (mut last, mut next) = (8i32, 8i32);
    for _ in 0..size {
        if next != 0 {
            next = (last + r.se()? + 256) % 256;
        }
        if next != 0 {
            last = next;
        }
    }
    Some(())
}

fn skip_hrd(r: &mut Reader) -> Option<()> {
    let cpb_cnt = r.ue()? + 1;
    r.bits(8)?; // bit_rate_scale, cpb_size_scale
    for _ in 0..cpb_cnt {
        r.ue()?;
        r.ue()?;
        r.bit()?;
    }
    r.bits(20)?; // four 5-bit length fields
    Some(())
}

/// Takes one SPS NAL unit (header byte included, no start code) and returns it with
/// `bitstream_restriction` set for zero-delay output. `None` if it cannot be parsed,
/// in which case the caller should send the original.
pub fn add_low_latency_vui(nal: &[u8]) -> Option<Vec<u8>> {
    if nal.first()? & 0x1f != 7 {
        return None;
    }
    let rbsp = unescape(nal);
    let mut r = Reader { data: &rbsp, pos: 8 };

    let profile = r.bits(8)?;
    r.bits(16)?; // constraint flags + level
    r.ue()?; // seq_parameter_set_id
    if matches!(profile, 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135) {
        let chroma = r.ue()?;
        if chroma == 3 {
            r.bit()?;
        }
        r.ue()?;
        r.ue()?;
        r.bit()?;
        if r.bit()? == 1 {
            for i in 0..if chroma == 3 { 12 } else { 8 } {
                if r.bit()? == 1 {
                    skip_scaling_list(&mut r, if i < 6 { 16 } else { 64 })?;
                }
            }
        }
    }
    r.ue()?; // log2_max_frame_num_minus4
    match r.ue()? {
        0 => {
            r.ue()?;
        }
        1 => {
            r.bit()?;
            r.se()?;
            r.se()?;
            for _ in 0..r.ue()? {
                r.se()?;
            }
        }
        _ => {}
    }
    let num_ref_frames = r.ue()?;
    r.bit()?; // gaps_in_frame_num_value_allowed_flag
    r.ue()?;
    r.ue()?;
    if r.bit()? == 0 {
        r.bit()?; // mb_adaptive_frame_field_flag
    }
    r.bit()?; // direct_8x8_inference_flag
    if r.bit()? == 1 {
        for _ in 0..4 {
            r.ue()?;
        }
    }

    // Everything up to `copy_until` is kept; then comes whatever we append.
    let vui_flag_pos = r.pos;
    let has_vui = r.bit()? == 1;
    let copy_until;
    if has_vui {
        if r.bit()? == 1 && r.bits(8)? == 255 {
            r.bits(32)?; // sar width/height
        }
        if r.bit()? == 1 {
            r.bit()?;
        }
        if r.bit()? == 1 {
            r.bits(4)?;
            if r.bit()? == 1 {
                r.bits(24)?;
            }
        }
        if r.bit()? == 1 {
            r.ue()?;
            r.ue()?;
        }
        if r.bit()? == 1 {
            r.bits(32)?;
            r.bits(32)?;
            r.bit()?;
        }
        let nal_hrd = r.bit()? == 1;
        if nal_hrd {
            skip_hrd(&mut r)?;
        }
        let vcl_hrd = r.bit()? == 1;
        if vcl_hrd {
            skip_hrd(&mut r)?;
        }
        if nal_hrd || vcl_hrd {
            r.bit()?; // low_delay_hrd_flag
        }
        r.bit()?; // pic_struct_present_flag
        copy_until = r.pos; // position of bitstream_restriction_flag
    } else {
        copy_until = vui_flag_pos;
    }
    if copy_until > rbsp.len() * 8 {
        return None;
    }

    let mut w = Writer { out: Vec::with_capacity(rbsp.len() + 8), bits: 0 };
    let mut src = Reader { data: &rbsp, pos: 0 };
    for _ in 0..copy_until {
        w.bit(src.bit()?);
    }
    if !has_vui {
        w.bit(1); // vui_parameters_present_flag
        w.bits(0, 8); // aspect, overscan, signal type, chroma loc, timing, nal/vcl hrd, pic_struct
    }
    w.bit(1); // bitstream_restriction_flag
    w.bit(1); // motion_vectors_over_pic_boundaries_flag
    w.ue(2); // max_bytes_per_pic_denom (default)
    w.ue(1); // max_bits_per_mb_denom (default)
    w.ue(16); // log2_max_mv_length_horizontal
    w.ue(16); // log2_max_mv_length_vertical
    w.ue(0); // max_num_reorder_frames
    w.ue(num_ref_frames.max(1)); // max_dec_frame_buffering
    w.bit(1); // rbsp_stop_one_bit
    while !w.bits.is_multiple_of(8) {
        w.bit(0);
    }
    Some(escape(&w.out))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parses the fields a test cares about, following the same syntax as above.
    fn restriction(nal: &[u8]) -> (u32, u32, u32, u32, u32) {
        let rbsp = unescape(nal);
        let mut r = Reader { data: &rbsp, pos: 8 };
        let profile = r.bits(8).unwrap();
        r.bits(16).unwrap();
        r.ue().unwrap();
        assert_eq!(profile, 66, "test helper only handles baseline");
        r.ue().unwrap();
        assert_eq!(r.ue().unwrap(), 2);
        let refs = r.ue().unwrap();
        r.bit().unwrap();
        let w = r.ue().unwrap();
        let h = r.ue().unwrap();
        assert_eq!(r.bit().unwrap(), 1);
        r.bit().unwrap();
        assert_eq!(r.bit().unwrap(), 0);
        assert_eq!(r.bit().unwrap(), 1, "vui present");
        assert_eq!(r.bits(8).unwrap(), 0);
        assert_eq!(r.bit().unwrap(), 1, "bitstream_restriction_flag");
        r.bit().unwrap();
        for _ in 0..4 {
            r.ue().unwrap();
        }
        (refs, w, h, r.ue().unwrap(), r.ue().unwrap())
    }

    #[test]
    fn adds_vui_to_minimal_sps() {
        // baseline, level 4.0, poc type 2, 1 ref, 120x68 MBs, frame_mbs_only, no VUI
        let mut w = Writer { out: vec![], bits: 0 };
        w.bits(0x67, 8);
        w.bits(66, 8);
        w.bits(0xc0, 8);
        w.bits(40, 8);
        w.ue(0);
        w.ue(0);
        w.ue(2);
        w.ue(1);
        w.bit(0);
        w.ue(119);
        w.ue(67);
        w.bit(1);
        w.bit(1);
        w.bit(0);
        w.bit(0);
        w.bit(1);
        while !w.bits.is_multiple_of(8) {
            w.bit(0);
        }
        let out = add_low_latency_vui(&escape(&w.out)).unwrap();
        assert_eq!(restriction(&out), (1, 119, 67, 0, 1));
    }

    #[test]
    fn escaping_roundtrips() {
        let raw = [0x67, 0, 0, 1, 0, 0, 0, 0, 3, 5];
        let esc = escape(&raw);
        assert_eq!(esc, vec![0x67, 0, 0, 3, 1, 0, 0, 3, 0, 0, 3, 3, 5]);
        assert_eq!(unescape(&esc), raw);
    }

    #[test]
    fn rejects_non_sps() {
        assert!(add_low_latency_vui(&[0x68, 0xce, 0x3c, 0x80]).is_none());
    }
}
