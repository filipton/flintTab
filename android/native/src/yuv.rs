//! The screen as NV12 (Y plane, then interleaved Cb Cr), handed to the display as YUV.
//!
//! RGBA buffers cost the display controller 4 bytes per pixel to read, 13 MB per frame here:
//! at 90 Hz it fell behind (FIFO underrun) and the end of each panel line, the upper half of
//! the landscape screen, showed gray. The video path the system itself uses reads compressed
//! YUV. NV12 is 1.5 bytes per pixel, the tablet's tiles and decoded frames already are NV12,
//! and the display rotates and converts it itself: updates are plain row copies.

use crate::front::{AHB, AHardwareBuffer_acquire, AHardwareBuffer_fromHardwareBuffer, AHardwareBuffer_lockPlanes,
    AHardwareBuffer_release, AHardwareBuffer_unlock, Planes, Rect, Yuv};
use jni_sys::{JNIEnv, jobject};

const CPU_WRITE_RARELY: u64 = 2 << 4;

pub struct Screen {
    w: usize,
    h: usize,
    y: Vec<u8>,
    uv: Vec<u8>, // Cb Cr pairs, w bytes per row, h / 2 rows
    /// Swap-chain buffers and, per buffer, the areas it is missing.
    buffers: Vec<(*mut AHB, Vec<Rect>)>,
}

unsafe impl Send for Screen {}

/// Even bounds (chroma covers 2x2 pixels), clipped to the screen.
fn chroma_aligned(r: Rect, w: usize, h: usize) -> Rect {
    Rect { x0: r.x0 & !1, y0: r.y0 & !1, x1: (r.x1.min(w) + 1) & !1, y1: (r.y1.min(h) + 1) & !1 }
}

impl Screen {
    pub fn new(w: usize, h: usize) -> Self {
        // Black in video range: Y 16, Cb Cr 128.
        Self { w, h, y: vec![16; w * h], uv: vec![128; w * h / 2], buffers: Vec::new() }
    }

    pub unsafe fn add_buffer(&mut self, env: *mut JNIEnv, hb: jobject) -> i32 {
        unsafe {
            let b = AHardwareBuffer_fromHardwareBuffer(env, hb);
            if b.is_null() {
                return -1;
            }
            AHardwareBuffer_acquire(b);
            self.buffers.push((b, vec![Rect { x0: 0, y0: 0, x1: self.w, y1: self.h }]));
        }
        self.buffers.len() as i32 - 1
    }

    /// Copies the screen rectangle `r` from `src` (whose pixel (0, 0) is screen pixel (ox, oy)).
    pub fn update(&mut self, src: &Yuv, r: Rect) {
        let r = chroma_aligned(r, self.w, self.h);
        if r.x0 >= r.x1 || r.y0 >= r.y1 || r.x0 < src.ox || r.y0 < src.oy {
            return;
        }
        let n = r.x1 - r.x0;
        unsafe {
            for y in r.y0..r.y1 {
                let from = src.y.add((y - src.oy) * src.y_stride + (r.x0 - src.ox));
                std::ptr::copy_nonoverlapping(from, self.y.as_mut_ptr().add(y * self.w + r.x0), n);
            }
            let interleaved = src.uv_step == 2 && (src.u as usize).abs_diff(src.v as usize) == 1;
            for cy in r.y0 / 2..r.y1 / 2 {
                let sy = cy - src.oy / 2;
                let out = self.uv.as_mut_ptr().add(cy * self.w + r.x0);
                if interleaved && src.u < src.v {
                    // NV12 to NV12: one copy per row.
                    std::ptr::copy_nonoverlapping(src.u.add(sy * src.uv_stride + (r.x0 - src.ox)), out, n);
                } else {
                    for i in 0..n / 2 {
                        let c = sy * src.uv_stride + ((r.x0 - src.ox) / 2 + i) * src.uv_step;
                        *out.add(2 * i) = *src.u.add(c);
                        *out.add(2 * i + 1) = *src.v.add(c);
                    }
                }
            }
        }
        for (_, stale) in &mut self.buffers {
            add_merged(stale, r);
        }
    }

    /// Brings buffer `i` (not on screen) up to date with the areas it is missing.
    pub fn render(&mut self, i: usize) -> bool {
        let Some((buf, stale)) = self.buffers.get_mut(i) else { return false };
        let (buf, rects) = (*buf, std::mem::take(stale));
        if rects.is_empty() {
            return true;
        }
        unsafe {
            let mut p: Planes = std::mem::zeroed();
            if AHardwareBuffer_lockPlanes(buf, CPU_WRITE_RARELY, -1, std::ptr::null(), &mut p) != 0 || p.count < 3 {
                crate::front::log("yuv buffer lock failed");
                return false;
            }
            let (y, cb, cr) = (&p.planes[0], &p.planes[1], &p.planes[2]);
            let nv12 = cb.pixel_stride == 2 && cr.data as usize == cb.data as usize + 1;
            let nv21 = cb.pixel_stride == 2 && cb.data as usize == cr.data as usize + 1;
            for r in rects {
                let n = r.x1 - r.x0;
                for row in r.y0..r.y1 {
                    std::ptr::copy_nonoverlapping(
                        self.y.as_ptr().add(row * self.w + r.x0),
                        y.data.add(row * y.row_stride as usize + r.x0 * y.pixel_stride as usize),
                        n,
                    );
                }
                for cy in r.y0 / 2..r.y1 / 2 {
                    let from = self.uv.as_ptr().add(cy * self.w + r.x0);
                    if nv12 {
                        std::ptr::copy_nonoverlapping(from, cb.data.add(cy * cb.row_stride as usize + r.x0), n);
                    } else {
                        let base = if nv21 { cr.data } else { cb.data };
                        for i in 0..n / 2 {
                            let (u, v) = (*from.add(2 * i), *from.add(2 * i + 1));
                            if nv21 {
                                let o = base.add(cy * cr.row_stride as usize + r.x0 + 2 * i);
                                *o = v;
                                *o.add(1) = u;
                            } else {
                                *cb.data.add(cy * cb.row_stride as usize + (r.x0 / 2 + i) * cb.pixel_stride as usize) = u;
                                *cr.data.add(cy * cr.row_stride as usize + (r.x0 / 2 + i) * cr.pixel_stride as usize) = v;
                            }
                        }
                    }
                }
            }
            AHardwareBuffer_unlock(buf, std::ptr::null_mut());
        }
        true
    }

    /// The picture as RGBA (debugging).
    pub fn dump(&self, out: &mut [u32]) {
        for y in 0..self.h {
            for x in 0..self.w {
                let c = (y / 2) * self.w + (x & !1);
                out[y * self.w + x] = crate::neon::scalar(self.y[y * self.w + x], self.uv[c], self.uv[c + 1]);
            }
        }
    }

    pub fn size(&self) -> (usize, usize) {
        (self.w, self.h)
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        for (b, _) in &self.buffers {
            unsafe { AHardwareBuffer_release(*b) };
        }
    }
}

/// Adds `r` to `list`, merged with areas it overlaps or nearly touches.
fn add_merged(list: &mut Vec<Rect>, mut r: Rect) {
    const NEAR: usize = 32;
    loop {
        let near = list.iter().position(|p| {
            p.x0 <= r.x1 + NEAR && r.x0 <= p.x1 + NEAR && p.y0 <= r.y1 + NEAR && r.y0 <= p.y1 + NEAR
        });
        match near {
            Some(i) => r = r.union(list.swap_remove(i)),
            None => break,
        }
    }
    list.push(r);
}
