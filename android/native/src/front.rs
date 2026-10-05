//! The front buffer: one RGBA AHardwareBuffer the panel scans out directly (a hardware overlay),
//! written by the CPU. Screen updates, the cursor sprite and the rotation into the panel's
//! native orientation all happen here; no GPU, no compositor round trip.

use jni_sys::{JNIEnv, jobject};
use std::ffi::c_void;

#[repr(C)]
#[derive(Default)]
pub struct Desc {
    pub width: u32,
    pub height: u32,
    pub layers: u32,
    pub format: u32,
    pub usage: u64,
    pub stride: u32,
    pub rfu0: u32,
    pub rfu1: u64,
}

#[repr(C)]
pub struct ARect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

pub enum AHB {}

#[link(name = "android")]
unsafe extern "C" {
    pub fn AHardwareBuffer_fromHardwareBuffer(env: *mut JNIEnv, hb: jobject) -> *mut AHB;
    pub fn AHardwareBuffer_acquire(b: *mut AHB);
    pub fn AHardwareBuffer_release(b: *mut AHB);
    pub fn AHardwareBuffer_describe(b: *const AHB, d: *mut Desc);
    pub fn AHardwareBuffer_lock(b: *mut AHB, usage: u64, fence: i32, rect: *const ARect, out: *mut *mut c_void) -> i32;
    pub fn AHardwareBuffer_unlock(b: *mut AHB, fence: *mut i32) -> i32;
}

#[link(name = "log")]
unsafe extern "C" {
    fn __android_log_write(prio: i32, tag: *const u8, text: *const u8) -> i32;
}

pub fn log(msg: &str) {
    let text = std::ffi::CString::new(msg).unwrap_or_default();
    unsafe { __android_log_write(4, c"tabdisplay".as_ptr() as *const u8, text.as_ptr() as *const u8) };
}

pub const CPU_WRITE_OFTEN: u64 = 3 << 4;

/// How view coordinates (the app's landscape view) land in the buffer (the panel's own
/// orientation): the buffer transform hint the buffer was allocated for.
#[derive(Clone, Copy)]
pub enum Rotation {
    R0,
    R90,
    R180,
    R270,
}

impl Rotation {
    /// From SurfaceControl.BUFFER_TRANSFORM_* (0, ROTATE_90 = 4, ROTATE_180 = 3, ROTATE_270 = 7).
    pub fn from_hint(h: i32) -> Self {
        match h {
            4 => Rotation::R90,
            3 => Rotation::R180,
            7 => Rotation::R270,
            _ => Rotation::R0,
        }
    }
}

pub struct Front {
    pub buf: *mut AHB,
    pub bw: usize,
    pub bh: usize,
    pub stride: usize,
    pub vw: usize,
    pub vh: usize,
    pub rot: Rotation,
    /// The screen without the cursor, in view orientation, in ordinary memory. Updates are
    /// converted into it row by row (cache friendly), then copied to the front buffer with the
    /// cursor composited on the way. The front buffer is never read, so it is locked write-only.
    shadow: Vec<u32>,
    cursor: Cursor,
}

unsafe impl Send for Front {}
// Worker threads only read its geometry and write disjoint rows.
unsafe impl Sync for Front {}

/// The computer's cursor, composited over the shadow whenever its pixels are copied out.
#[derive(Default)]
struct Cursor {
    image: Vec<u32>, // premultiplied RGBA, w x h
    w: usize,
    h: usize,
    x: i64,
    y: i64,
    shown: bool,
    /// Where it was last drawn (view rect).
    drawn: Option<Rect>,
}

/// A view rectangle [x0, x1) x [y0, y1).
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Rect {
    pub x0: usize,
    pub y0: usize,
    pub x1: usize,
    pub y1: usize,
}

impl Rect {
    fn union(self, o: Rect) -> Rect {
        Rect { x0: self.x0.min(o.x0), y0: self.y0.min(o.y0), x1: self.x1.max(o.x1), y1: self.y1.max(o.y1) }
    }
    fn is_empty(self) -> bool {
        self.x0 >= self.x1 || self.y0 >= self.y1
    }
    fn area(self) -> usize {
        if self.is_empty() { 0 } else { (self.x1 - self.x0) * (self.y1 - self.y0) }
    }
}

/// A YUV 4:2:0 source picture: view pixel (x, y) is source pixel (x - ox, y - oy).
pub struct Yuv {
    pub y: *const u8,
    pub y_stride: usize,
    pub u: *const u8,
    pub v: *const u8,
    pub uv_stride: usize,
    pub uv_step: usize, // 2 for interleaved (NV12/NV21), 1 for planar
    pub ox: usize,
    pub oy: usize,
}

unsafe impl Sync for Yuv {}

/// BT.709 video range to RGBA (bytes R, G, B, A): the scalar twin of the NEON path.
#[inline(always)]
fn rgba(y: u8, u: u8, v: u8) -> u32 {
    crate::neon::scalar(y, u, v)
}

/// Premultiplied `s` over opaque `d`.
#[inline(always)]
fn over(s: u32, d: u32) -> u32 {
    let a = s >> 24;
    if a == 0 {
        return d;
    }
    if a == 255 {
        return s;
    }
    let inv = 255 - a;
    let ch = |sh: u32| ((((s >> sh) & 0xff) + (((d >> sh) & 0xff) * inv + 127) / 255).min(255)) << sh;
    ch(0) | ch(8) | ch(16) | 0xff00_0000
}

/// Raw pointer that may cross into worker threads (each writes its own rows).
#[derive(Clone, Copy)]
struct Ptr(*mut u32);
unsafe impl Send for Ptr {}
unsafe impl Sync for Ptr {}

/// Moves the calling thread to the performance cores (4-7 on this Exynos; the efficiency cores
/// run this code 2-3x slower and the scheduler likes to leave short bursts there).
pub fn pin_to_big_cores() {
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        let n = libc::sysconf(libc::_SC_NPROCESSORS_CONF).max(1) as usize;
        for cpu in n / 2..n {
            libc::CPU_SET(cpu, &mut set);
        }
        libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set);
    }
}

thread_local! {
    static PINNED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn pin_once() {
    PINNED.with(|p| {
        if !p.get() {
            pin_to_big_cores();
            p.set(true);
        }
    });
}

/// Runs `f` over `rows`, split across threads when `pixels` is large.
fn rows_parallel(rows: std::ops::Range<usize>, pixels: usize, f: impl Fn(std::ops::Range<usize>) + Sync) {
    pin_once();
    // Spawning threads costs ~0.1 ms: only worth it for big areas.
    let threads = if pixels > 400_000 { 4 } else { 1 };
    if threads == 1 {
        return f(rows);
    }
    let n = rows.len().div_ceil(threads);
    std::thread::scope(|s| {
        for t in 0..threads {
            let part = rows.start + t * n..(rows.start + (t + 1) * n).min(rows.end);
            if part.is_empty() {
                continue;
            }
            let f = &f;
            s.spawn(move || {
                pin_to_big_cores();
                f(part)
            });
        }
    });
}

const BLOCK: usize = 32;

impl Front {
    pub unsafe fn attach(env: *mut JNIEnv, hb: jobject, hint: i32) -> Option<Box<Front>> {
        unsafe {
            let buf = AHardwareBuffer_fromHardwareBuffer(env, hb);
            if buf.is_null() {
                return None;
            }
            AHardwareBuffer_acquire(buf);
            let mut d = Desc::default();
            AHardwareBuffer_describe(buf, &mut d);
            let rot = Rotation::from_hint(hint);
            let (bw, bh) = (d.width as usize, d.height as usize);
            let (vw, vh) = match rot {
                Rotation::R90 | Rotation::R270 => (bh, bw),
                _ => (bw, bh),
            };
            let shadow = vec![0xff00_0000; vw * vh];
            Some(Box::new(Front { buf, bw, bh, stride: d.stride as usize, vw, vh, rot, shadow, cursor: Cursor::default() }))
        }
    }

    /// Buffer pixel for view pixel (x, y).
    #[inline(always)]
    pub fn map(&self, x: usize, y: usize) -> (usize, usize) {
        match self.rot {
            Rotation::R0 => (x, y),
            Rotation::R90 => (self.bw - 1 - y, x),
            Rotation::R180 => (self.bw - 1 - x, self.bh - 1 - y),
            Rotation::R270 => (y, self.bh - 1 - x),
        }
    }

    /// View pixel for buffer pixel (bx, by).
    #[inline(always)]
    fn unmap(&self, bx: usize, by: usize) -> (usize, usize) {
        match self.rot {
            Rotation::R0 => (bx, by),
            Rotation::R90 => (by, self.bw - 1 - bx),
            Rotation::R180 => (self.bw - 1 - bx, self.bh - 1 - by),
            Rotation::R270 => (self.bh - 1 - by, bx),
        }
    }

    fn clip(&self, r: Rect) -> Rect {
        Rect { x0: r.x0.min(self.vw), y0: r.y0.min(self.vh), x1: r.x1.min(self.vw), y1: r.y1.min(self.vh) }
    }

    /// The buffer rectangle covering view rectangle `r`.
    fn buffer_rect(&self, r: Rect) -> ARect {
        let (ax, ay) = self.map(r.x0, r.y0);
        let (bx, by) = self.map(r.x1 - 1, r.y1 - 1);
        ARect { left: ax.min(bx) as i32, top: ay.min(by) as i32, right: ax.max(bx) as i32 + 1, bottom: ay.max(by) as i32 + 1 }
    }

    fn cursor_rect(&self) -> Option<Rect> {
        let c = &self.cursor;
        if !c.shown || c.w == 0 {
            return None;
        }
        let r = self.clip(Rect {
            x0: c.x.max(0) as usize,
            y0: c.y.max(0) as usize,
            x1: (c.x + c.w as i64).max(0) as usize,
            y1: (c.y + c.h as i64).max(0) as usize,
        });
        (!r.is_empty()).then_some(r)
    }

    /// Copies view rectangle `r` of the shadow to the front buffer, with the cursor on top,
    /// in 32x32 blocks so both the reads and the rotated writes stay in cache.
    fn present(&self, r: Rect) -> bool {
        let r = self.clip(r);
        if r.is_empty() {
            return true;
        }
        let rect = self.buffer_rect(r);
        let mut p: *mut c_void = std::ptr::null_mut();
        unsafe {
            if AHardwareBuffer_lock(self.buf, CPU_WRITE_OFTEN, -1, &rect, &mut p) != 0 || p.is_null() {
                log("front lock failed");
                return false;
            }
        }
        let base = Ptr(p as *mut u32);
        if let Rotation::R90 = self.rot {
            self.present_r90(base, r);
        } else {
            self.present_any(base, rect, r);
        }
        self.present_cursor(base, r);
        unsafe { AHardwareBuffer_unlock(self.buf, std::ptr::null_mut()) };
        true
    }

    /// The landscape case: NEON 4x4 transposes for the aligned inside of `r`, plain code for
    /// the border strips.
    fn present_r90(&self, base: Ptr, r: Rect) {
        let (ax0, ax1) = (r.x0.next_multiple_of(4), r.x1 / 4 * 4);
        let (ay0, ay1) = (r.y0.next_multiple_of(4), r.y1 / 4 * 4);
        let shadow = self.shadow.as_ptr() as usize;
        if ax0 < ax1 && ay0 < ay1 {
            let bands: Vec<usize> = (ay0..ay1).step_by(BLOCK).collect();
            rows_parallel(0..bands.len(), r.area(), |part| {
                let base = base;
                for bi in part {
                    let y = bands[bi];
                    let ye = (y + BLOCK).min(ay1);
                    let mut x = ax0;
                    while x < ax1 {
                        let xe = (x + BLOCK).min(ax1);
                        unsafe { crate::neon::r90_4x4(shadow as *const u32, self.vw, base.0, self.stride, self.bw, x, xe, y, ye) };
                        x = xe;
                    }
                }
            });
        }
        let strips = [
            Rect { x0: r.x0, y0: r.y0, x1: r.x1, y1: ay0.min(r.y1) },
            Rect { x0: r.x0, y0: ay1.max(ay0), x1: r.x1, y1: r.y1 },
            Rect { x0: r.x0, y0: ay0, x1: ax0.min(r.x1), y1: ay1 },
            Rect { x0: ax1.max(ax0), y0: ay0, x1: r.x1, y1: ay1 },
        ];
        for s in strips {
            if s.is_empty() {
                continue;
            }
            for y in s.y0..s.y1 {
                for x in s.x0..s.x1 {
                    let (bx, by) = self.map(x, y);
                    unsafe { *base.0.add(by * self.stride + bx) = self.shadow[y * self.vw + x] };
                }
            }
        }
    }

    /// Other rotations: blocks of the buffer rectangle, each written row by row in the
    /// buffer's own order (sequential writes into the scanned-out memory); the matching shadow
    /// pixels of a 32x32 block stay in cache.
    fn present_any(&self, base: Ptr, rect: ARect, r: Rect) {
        let rows: Vec<usize> = (rect.top as usize..rect.bottom as usize).step_by(BLOCK).collect();
        let (left, right, bottom) = (rect.left as usize, rect.right as usize, rect.bottom as usize);
        let shadow = self.shadow.as_ptr() as usize;
        rows_parallel(0..rows.len(), r.area(), |blocks| {
            let base = base;
            let shadow = shadow as *const u32;
            for bi in blocks {
                let by0 = rows[bi];
                let by1 = (by0 + BLOCK).min(bottom);
                let mut bx0 = left;
                while bx0 < right {
                    let bx1 = (bx0 + BLOCK).min(right);
                    for by in by0..by1 {
                        unsafe {
                            let out = base.0.add(by * self.stride);
                            for bx in bx0..bx1 {
                                let (x, y) = self.unmap(bx, by);
                                *out.add(bx) = *shadow.add(y * self.vw + x);
                            }
                        }
                    }
                    bx0 = bx1;
                }
            }
        });
    }

    /// The cursor over what was just copied, where they overlap.
    fn present_cursor(&self, base: Ptr, r: Rect) {
        let Some(cr) = self.cursor_rect() else { return };
        let c = &self.cursor;
        let o = Rect { x0: cr.x0.max(r.x0), y0: cr.y0.max(r.y0), x1: cr.x1.min(r.x1), y1: cr.y1.min(r.y1) };
        if o.is_empty() {
            return;
        }
        for y in o.y0..o.y1 {
            for x in o.x0..o.x1 {
                let px = over(c.image[(y as i64 - c.y) as usize * c.w + (x as i64 - c.x) as usize], self.shadow[y * self.vw + x]);
                let (bx, by) = self.map(x, y);
                unsafe { *base.0.add(by * self.stride + bx) = px };
            }
        }
    }

    /// Writes the view rectangle `r` of `src` and shows it.
    pub fn update(&mut self, src: &Yuv, r: Rect) -> bool {
        let r = self.clip(r);
        if r.is_empty() {
            return true;
        }
        let shadow = Ptr(self.shadow.as_mut_ptr());
        let vw = self.vw;
        rows_parallel(r.y0..r.y1, r.area(), |rows| {
            let shadow = shadow;
            for y in rows {
                let sy = y - src.oy;
                unsafe {
                    let yrow = src.y.add(sy * src.y_stride);
                    let crow = (sy / 2) * src.uv_stride;
                    let out = shadow.0.add(y * vw);
                    let mut x = r.x0;
                    // NEON for runs of 16 pixels starting on a chroma pair (interleaved chroma).
                    if src.uv_step == 2 && (src.u as usize).abs_diff(src.v as usize) == 1 {
                        if (x - src.ox) % 2 == 1 {
                            let sx = x - src.ox;
                            let c = crow + (sx / 2) * 2;
                            *out.add(x) = rgba(*yrow.add(sx), *src.u.add(c), *src.v.add(c));
                            x += 1;
                        }
                        let n = (r.x1 - x) / 16 * 16;
                        if n > 0 {
                            let sx = x - src.ox;
                            let uv = src.u.min(src.v).add(crow + sx);
                            crate::neon::row(yrow.add(sx), uv, src.v < src.u, out.add(x), n);
                            x += n;
                        }
                    }
                    while x < r.x1 {
                        let sx = x - src.ox;
                        let c = crow + (sx / 2) * src.uv_step;
                        let (u, v) = (*src.u.add(c), *src.v.add(c));
                        *out.add(x) = rgba(*yrow.add(sx), u, v);
                        // The pixel sharing this chroma sample, when it is in the rectangle.
                        if sx % 2 == 0 && x + 1 < r.x1 {
                            *out.add(x + 1) = rgba(*yrow.add(sx + 1), u, v);
                            x += 2;
                        } else {
                            x += 1;
                        }
                    }
                }
            }
        });
        self.present(r)
    }

    /// Fills a view rectangle with one RGBA colour.
    pub fn fill(&mut self, x0: usize, y0: usize, x1: usize, y1: usize, rgba: u32) {
        let r = self.clip(Rect { x0, y0, x1, y1 });
        if r.is_empty() {
            return;
        }
        for y in r.y0..r.y1 {
            self.shadow[y * self.vw + r.x0..y * self.vw + r.x1].fill(rgba);
        }
        self.present(r);
    }

    /// The current picture in view orientation (RGBA), for debugging.
    pub fn dump(&self, out: &mut [u32]) {
        out[..self.shadow.len()].copy_from_slice(&self.shadow);
        if let Some(cr) = self.cursor_rect() {
            let c = &self.cursor;
            for y in cr.y0..cr.y1 {
                for x in cr.x0..cr.x1 {
                    let i = y * self.vw + x;
                    out[i] = over(c.image[(y as i64 - c.y) as usize * c.w + (x as i64 - c.x) as usize], out[i]);
                }
            }
        }
    }

    /// A new cursor picture: premultiplied RGBA, `w` x `h` view pixels.
    pub fn set_cursor_image(&mut self, rgba: &[u32], w: usize, h: usize) {
        self.redraw_cursor(|c| {
            c.image = rgba.to_vec();
            c.w = w;
            c.h = h;
        });
    }

    /// Moves the cursor's top-left to (x, y) in view pixels.
    pub fn move_cursor(&mut self, x: i64, y: i64, shown: bool) {
        self.redraw_cursor(|c| {
            c.x = x;
            c.y = y;
            c.shown = shown;
        });
    }

    fn redraw_cursor(&mut self, change: impl FnOnce(&mut Cursor)) {
        let old = self.cursor.drawn;
        change(&mut self.cursor);
        let new = self.cursor_rect();
        self.cursor.drawn = new;
        match (old, new) {
            (Some(a), Some(b)) => {
                // Two small copies beat one big box when the cursor jumps.
                if a.union(b).area() > 2 * (a.area() + b.area()) {
                    self.present(a);
                    self.present(b);
                } else {
                    self.present(a.union(b));
                }
            }
            (Some(a), None) | (None, Some(a)) => {
                self.present(a);
            }
            (None, None) => {}
        }
    }
}

impl Drop for Front {
    fn drop(&mut self) {
        unsafe { AHardwareBuffer_release(self.buf) };
    }
}
