//! NV12 / NV21 row to RGBA with NEON, 16 pixels per step (BT.709 video range, 6-bit fixed point).
use std::arch::aarch64::*;

/// `n` pixels: luma at `y`, interleaved chroma pairs at `uv` (Cb first unless `swap`), RGBA to `out`.
/// `n` must be a multiple of 16; the caller converts the rest with the scalar path.
#[inline(always)]
pub unsafe fn row(y: *const u8, uv: *const u8, swap: bool, out: *mut u32, n: usize) {
    unsafe {
        let k16 = vdupq_n_s16(16);
        let k128 = vdupq_n_s16(128);
        let alpha = vdupq_n_u8(255);
        let mut i = 0;
        while i < n {
            let yv = vld1q_u8(y.add(i));
            let c2 = vld2_u8(uv.add(i)); // 8 chroma pairs for 16 pixels
            let (cb, cr) = if swap { (c2.1, c2.0) } else { (c2.0, c2.1) };
            // Each chroma sample covers two pixels.
            let cb16 = vcombine_u8(vzip1_u8(cb, cb), vzip2_u8(cb, cb));
            let cr16 = vcombine_u8(vzip1_u8(cr, cr), vzip2_u8(cr, cr));
            let conv = |yy: uint8x8_t, b: uint8x8_t, r: uint8x8_t| {
                let c = vmulq_n_s16(vsubq_s16(vreinterpretq_s16_u16(vmovl_u8(yy)), k16), 75);
                let d = vsubq_s16(vreinterpretq_s16_u16(vmovl_u8(b)), k128);
                let e = vsubq_s16(vreinterpretq_s16_u16(vmovl_u8(r)), k128);
                let rr = vqrshrun_n_s16::<6>(vqaddq_s16(c, vmulq_n_s16(e, 115)));
                let gg = vqrshrun_n_s16::<6>(vqsubq_s16(vqsubq_s16(c, vmulq_n_s16(d, 14)), vmulq_n_s16(e, 34)));
                let bb = vqrshrun_n_s16::<6>(vqaddq_s16(c, vmulq_n_s16(d, 135)));
                (rr, gg, bb)
            };
            let (r0, g0, b0) = conv(vget_low_u8(yv), vget_low_u8(cb16), vget_low_u8(cr16));
            let (r1, g1, b1) = conv(vget_high_u8(yv), vget_high_u8(cb16), vget_high_u8(cr16));
            vst4q_u8(out.add(i) as *mut u8, uint8x16x4_t(vcombine_u8(r0, r1), vcombine_u8(g0, g1), vcombine_u8(b0, b1), alpha));
            i += 16;
        }
    }
}

/// The same in plain code (6-bit fixed point, same rounding and saturation).
#[inline(always)]
pub fn scalar(y: u8, u: u8, v: u8) -> u32 {
    let c = ((y as i32 - 16) * 75).clamp(-32768, 32767);
    let d = u as i32 - 128;
    let e = v as i32 - 128;
    let sat16 = |x: i32| x.clamp(-32768, 32767);
    let out = |x: i32| ((x + 32) >> 6).clamp(0, 255) as u32;
    let r = out(sat16(c + sat16(115 * e)));
    let g = out(sat16(sat16(c - sat16(14 * d)) - sat16(34 * e)));
    let b = out(sat16(c + sat16(135 * d)));
    r | g << 8 | b << 16 | 0xff00_0000
}
// View -> panel-native copy for a 90 degree rotation (buffer (bx, by) = view (by, bw-1-bx)),
// 4x4 pixels per step with NEON transposes.


/// Copies view rows y0..y0+4*k and columns x0..x0+4*m (both multiples of 4) of `src`
/// (stride `vw`) into `dst` (stride `stride`, width `bw`).
#[inline(always)]
pub unsafe fn r90_4x4(src: *const u32, vw: usize, dst: *mut u32, stride: usize, bw: usize, x0: usize, x1: usize, y0: usize, y1: usize) {
    unsafe {
        // Rows from the bottom up: buffer columns then ascend, so each buffer row is written
        // front to back (write-combined memory merges ascending stores best).
        let mut y = y1;
        while y > y0 {
            y -= 4;
            let mut x = x0;
            while x < x1 {
                let s = src.add(y * vw + x);
                let r0 = vld1q_u32(s);
                let r1 = vld1q_u32(s.add(vw));
                let r2 = vld1q_u32(s.add(2 * vw));
                let r3 = vld1q_u32(s.add(3 * vw));
                // transpose: column j = pixels (x+j, y..y+3)
                let t01 = vtrnq_u32(r0, r1);
                let t23 = vtrnq_u32(r2, r3);
                let c0 = vcombine_u32(vget_low_u32(t01.0), vget_low_u32(t23.0));
                let c1 = vcombine_u32(vget_low_u32(t01.1), vget_low_u32(t23.1));
                let c2 = vcombine_u32(vget_high_u32(t01.0), vget_high_u32(t23.0));
                let c3 = vcombine_u32(vget_high_u32(t01.1), vget_high_u32(t23.1));
                // buffer row x+j, columns bw-1-(y+3) .. bw-1-y: the column reversed
                let rev = |v: uint32x4_t| { let r = vrev64q_u32(v); vextq_u32::<2>(r, r) };
                let bx = bw - 1 - (y + 3);
                vst1q_u32(dst.add(x * stride + bx), rev(c0));
                vst1q_u32(dst.add((x + 1) * stride + bx), rev(c1));
                vst1q_u32(dst.add((x + 2) * stride + bx), rev(c2));
                vst1q_u32(dst.add((x + 3) * stride + bx), rev(c3));
                x += 4;
            }
        }
    }
}
