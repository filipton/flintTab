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
#[derive(Clone, Copy)]
pub struct ARect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

#[repr(C)]
pub struct Plane {
    pub data: *mut u8,
    pub pixel_stride: u32,
    pub row_stride: u32,
}

#[repr(C)]
pub struct Planes {
    pub count: u32,
    pub planes: [Plane; 4],
}

pub enum AHB {}

#[link(name = "android")]
unsafe extern "C" {
    pub fn AHardwareBuffer_fromHardwareBuffer(env: *mut JNIEnv, hb: jobject) -> *mut AHB;
    pub fn AHardwareBuffer_acquire(b: *mut AHB);
    pub fn AHardwareBuffer_release(b: *mut AHB);
    pub fn AHardwareBuffer_describe(b: *const AHB, d: *mut Desc);
    pub fn AHardwareBuffer_lock(b: *mut AHB, usage: u64, fence: i32, rect: *const ARect, out: *mut *mut c_void) -> i32;
    pub fn AHardwareBuffer_lockPlanes(b: *mut AHB, usage: u64, fence: i32, rect: *const ARect, out: *mut Planes) -> i32;
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

pub const CPU_WRITE_RARELY: u64 = 2 << 4;

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
    /// A second, identical buffer the layer flips to every refresh (front-buffer variants).
    pub twin: Option<*mut AHB>,
    /// Swap chain (the default): buffers handed to the compositor in turn, each written only
    /// while the display is not showing it. Per buffer: the areas it is missing.
    pub chain: Vec<(*mut AHB, Vec<Rect>)>,
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
    /// Changed since the last present (view rect).
    /// Separate areas changed since the last present (view rects; near ones merged).
    pending: Vec<Rect>,
    /// The panel's scan: a vsync time (CLOCK_MONOTONIC ns) and the refresh period.
    vsync_ns: i64,
    period_ns: i64,
    /// How long presenting takes per pixel (running average), to plan around the scan.
    ns_per_px: std::cell::Cell<f64>,
    /// Smoothness: frames shown per panel refresh (refresh index -> frames), logged every 5 s.
    pacing: std::collections::BTreeMap<i64, u32>,
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
    pub fn union(self, o: Rect) -> Rect {
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
/// Share of a refresh the panel spends scanning rows (the rest is blanking).
const SCAN_FRACTION: f64 = 0.97;
/// Margin for the uncertainty of where the scan is.
const GUARD_NS: f64 = 400_000.0;

pub fn now_ns() -> i64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec * 1_000_000_000 + ts.tv_nsec
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

/// Sleeps until shortly before `t`, then spins to it (sleep alone overshoots by ~0.1 ms).
fn wait_until(t: i64) {
    let left = t - now_ns();
    if left > 300_000 {
        std::thread::sleep(std::time::Duration::from_nanos((left - 200_000) as u64));
    }
    while now_ns() < t {
        std::hint::spin_loop();
    }
}

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
            Some(Box::new(Front {
                buf,
                twin: None,
                chain: Vec::new(),
                bw,
                bh,
                stride: d.stride as usize,
                vw,
                vh,
                rot,
                shadow,
                cursor: Cursor::default(),
                pending: Vec::new(),
                vsync_ns: 0,
                period_ns: 0,
                ns_per_px: std::cell::Cell::new(3.0),
                pacing: std::collections::BTreeMap::new(),
            }))
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
        let mut ok = true;
        for buf in std::iter::once(self.buf).chain(self.twin) {
            ok &= self.present_into(buf, r);
        }
        ok
    }

    fn present_into(&self, buf: *mut AHB, r: Rect) -> bool {
        // A swap-chain buffer is never on screen while written: the unlock's own cache
        // maintenance is enough there.
        let clean = self.chain.is_empty();
        let rect = self.buffer_rect(r);
        let mut p: *mut c_void = std::ptr::null_mut();
        unsafe {
            if AHardwareBuffer_lock(buf, CPU_WRITE_RARELY, -1, &rect, &mut p) != 0 || p.is_null() {
                log("front lock failed");
                return false;
            }
        }
        let base = Ptr(p as *mut u32);
        let started = now_ns();
        if let Rotation::R90 = self.rot {
            self.present_r90(base, r);
        } else {
            self.present_any(base, rect, r);
        }
        self.present_cursor(base, r);
        if clean {
            self.clean_to_memory(base, rect);
        }
        unsafe { AHardwareBuffer_unlock(buf, std::ptr::null_mut()) };
        if r.area() > 20_000 {
            let per_px = (now_ns() - started) as f64 / r.area() as f64;
            self.ns_per_px.set(self.ns_per_px.get() * 0.8 + per_px * 0.2);
        }
        true
    }

    /// Pushes the written rows out of the CPU caches to memory, where the display controller
    /// reads them. The allocator may map the buffer cached and only clean part of it on unlock:
    /// lines left in the cache showed on the panel as gray stripes of older pictures. Does
    /// nothing on an uncached mapping.
    fn clean_to_memory(&self, base: Ptr, rect: ARect) {
        const LINE: usize = 64;
        let (left, right) = (rect.left as usize * 4, rect.right as usize * 4);
        for by in rect.top as usize..rect.bottom as usize {
            let row = unsafe { (base.0 as *mut u8).add(by * self.stride * 4) } as usize;
            let mut a = (row + left) & !(LINE - 1);
            while a < row + right {
                unsafe { std::arch::asm!("dc cvac, {0}", in(reg) a, options(nostack, preserves_flags)) };
                a += LINE;
            }
        }
        unsafe { std::arch::asm!("dsb sy", options(nostack, preserves_flags)) };
    }

    /// The landscape case, in the panel's scan order (buffer row = view column, left to right),
    /// so a write started ahead of the scan stays ahead of it: NEON 4x4 transposes for whole
    /// 4-column groups, plain code for the edges. Large areas use 4 threads that take column
    /// groups in order, so the written front still advances like the scan.
    fn present_r90(&self, base: Ptr, r: Rect) {
        let (ay0, ay1) = (r.y0.next_multiple_of(4), r.y1 / 4 * 4);
        let shadow = self.shadow.as_ptr() as usize;
        let scalar = |base: Ptr, x0: usize, x1: usize, y0: usize, y1: usize| {
            for x in x0..x1 {
                for y in y0..y1 {
                    let (bx, by) = self.map(x, y);
                    unsafe { *base.0.add(by * self.stride + bx) = self.shadow[y * self.vw + x] };
                }
            }
        };
        // One group = up to 4 view columns = up to 4 buffer rows.
        let group = |base: Ptr, x: usize| {
            let xe = (x + 4).min(r.x1);
            if x % 4 == 0 && xe - x == 4 && ay0 < ay1 {
                unsafe { crate::neon::r90_4x4(shadow as *const u32, self.vw, base.0, self.stride, self.bw, x, xe, ay0, ay1) };
                scalar(base, x, xe, r.y0, ay0.min(r.y1));
                scalar(base, x, xe, ay1.max(ay0), r.y1);
            } else {
                scalar(base, x, xe, r.y0, r.y1);
            }
        };
        // Groups: a partial one up to the first multiple of 4, then whole ones.
        let mut starts = vec![r.x0];
        let mut x = r.x0.next_multiple_of(4).max(r.x0 + 1).min(r.x1);
        if r.x0 % 4 == 0 {
            x = r.x0 + 4;
        }
        while x < r.x1 {
            starts.push(x);
            x += 4;
        }
        if r.area() > 300_000 {
            let next = std::sync::atomic::AtomicUsize::new(0);
            std::thread::scope(|s| {
                for _ in 0..4 {
                    s.spawn(|| {
                        pin_to_big_cores();
                        loop {
                            // Chunks of 4 groups, handed out in scan order.
                            let i = next.fetch_add(4, std::sync::atomic::Ordering::Relaxed);
                            if i >= starts.len() {
                                break;
                            }
                            for &x in &starts[i..(i + 4).min(starts.len())] {
                                group(base, x);
                            }
                        }
                    });
                }
            });
        } else {
            for &x in &starts {
                group(base, x);
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
        self.mark(r);
        true
    }

    /// Adds a changed area. Areas stay separate unless they overlap or nearly touch: one box
    /// around a bar's old and new place on opposite edges would be the whole screen.
    fn mark(&mut self, r: Rect) {
        let mut r = self.clip(r);
        if r.is_empty() {
            return;
        }
        for (_, stale) in &mut self.chain {
            add_merged(stale, r);
        }
        const NEAR: usize = 32;
        loop {
            let near = self.pending.iter().position(|p| {
                p.x0 <= r.x1 + NEAR && r.x0 <= p.x1 + NEAR && p.y0 <= r.y1 + NEAR && r.y0 <= p.y1 + NEAR
            });
            match near {
                Some(i) => r = r.union(self.pending.swap_remove(i)),
                None => break,
            }
        }
        self.pending.push(r);
    }

    /// Adds a swap-chain buffer (same size and format); returns its index. It starts out missing
    /// the whole picture.
    pub unsafe fn add_chain_buffer(&mut self, env: *mut JNIEnv, hb: jobject) -> i32 {
        unsafe {
            let b = AHardwareBuffer_fromHardwareBuffer(env, hb);
            if b.is_null() {
                return -1;
            }
            AHardwareBuffer_acquire(b);
            self.chain.push((b, vec![Rect { x0: 0, y0: 0, x1: self.vw, y1: self.vh }]));
        }
        self.chain.len() as i32 - 1
    }

    /// Brings swap-chain buffer `i` (not on screen) up to date: copies the areas it is missing
    /// from the shadow, cursor included. Returns false if it could not be locked.
    pub fn render_chain(&mut self, i: usize) -> bool {
        let Some((buf, stale)) = self.chain.get_mut(i) else { return false };
        let (buf, rects) = (*buf, std::mem::take(stale));
        self.pending.clear(); // the front-buffer path's list is not used with a chain
        let mut ok = true;
        for r in rects {
            ok &= self.present_into(buf, r);
        }
        ok
    }

    /// Adds the second buffer (same size and format) and copies the current picture into it.
    pub unsafe fn add_twin(&mut self, env: *mut JNIEnv, hb: jobject) -> bool {
        unsafe {
            let b = AHardwareBuffer_fromHardwareBuffer(env, hb);
            if b.is_null() {
                return false;
            }
            AHardwareBuffer_acquire(b);
            self.twin = Some(b);
        }
        self.present_into(self.twin.unwrap(), Rect { x0: 0, y0: 0, x1: self.vw, y1: self.vh })
    }

    /// The panel's scan timing: `vsync_ns` a time the scan starts at buffer row 0
    /// (CLOCK_MONOTONIC), `period_ns` the refresh period.
    pub fn set_vsync(&mut self, vsync_ns: i64, period_ns: i64) {
        if self.period_ns == 0 {
            log(&format!("vsync: period {:.2} ms, next in {:.2} ms", period_ns as f64 / 1e6, (vsync_ns - now_ns()) as f64 / 1e6));
        }
        self.vsync_ns = vsync_ns;
        self.period_ns = period_ns;
    }

    /// Shows everything changed since the last call, without tearing: the write starts only
    /// when the scan is not inside the changed buffer rows and will not catch up with the
    /// write before it ends, so each update reaches the panel whole within one refresh.
    /// Counts a shown frame (not cursor moves) towards the smoothness log.
    pub fn count_frame(&mut self) {
        if self.period_ns <= 0 {
            return;
        }
        // The refresh whose scan will show it: the next scan start after now.
        let phase = self.vsync_ns.rem_euclid(self.period_ns);
        let refresh = (now_ns() - phase).div_euclid(self.period_ns);
        *self.pacing.entry(refresh).or_default() += 1;
        let first = *self.pacing.keys().next().unwrap();
        if (refresh - first) * self.period_ns >= 5_000_000_000 {
            // Refreshes in the window with 0, 1 and 2+ frames (0 only between busy ones).
            let (mut zero, mut one, mut more) = (0, 0, 0);
            for r in first..refresh {
                match self.pacing.get(&r).copied().unwrap_or(0) {
                    0 => zero += 1,
                    1 => one += 1,
                    _ => more += 1,
                }
            }
            log(&format!("pacing: {one} refreshes with 1 frame, {more} with 2+, {zero} with none"));
            self.pacing.clear();
        }
    }

    pub fn present_pending(&mut self) -> bool {
        let mut rects: Vec<Rect> = std::mem::take(&mut self.pending);
        if rects.is_empty() {
            return true;
        }
        // In scan order (buffer rows ascending), written one after the other.
        rects.sort_by_key(|r| self.buffer_rect(*r).top);
        let jobs: Vec<(f64, f64, f64)> = rects
            .iter()
            .map(|r| {
                let b = self.buffer_rect(*r);
                (b.top as f64, b.bottom as f64, r.area() as f64 * self.ns_per_px.get())
            })
            .collect();
        if let Some(start) = self.plan(&jobs) {
            wait_until(start);
        }
        let mut ok = true;
        for r in rects {
            ok &= self.present(r);
        }
        ok
    }

    /// When to start writing the areas `jobs` (buffer rows [a, b) and how long each takes,
    /// written back to back) so that the scan is in none of them while it is written and does
    /// not catch up with its writing: every area then reaches the panel whole, all in the same
    /// refresh. None: start now (no scan timing yet, or no such moment soon).
    fn plan(&self, jobs: &[(f64, f64, f64)]) -> Option<i64> {
        if self.period_ns <= 0 {
            return None;
        }
        let period = self.period_ns as f64;
        let active = period * SCAN_FRACTION; // the rest is blanking
        let rows = self.bh as f64;
        let guard = GUARD_NS / active * rows;
        let phase_of_row = |row: f64| row / rows * active;
        let ok_at = |t: f64, a: f64, b: f64, duration: f64| {
            let (a, b) = ((a - guard).max(0.0), (b + guard).min(rows));
            let phase = (t - self.vsync_ns as f64).rem_euclid(period);
            let scanning = phase < active;
            let beam = phase / active * rows;
            let inside = scanning && beam >= a && beam < b;
            // Until the scan next reaches row b.
            let until_b = if scanning && beam < b { phase_of_row(b) - phase } else { period - phase + phase_of_row(b) };
            !inside && until_b >= duration + GUARD_NS
        };
        let now = now_ns();
        // Candidates every 0.1 ms over the next 1.5 refreshes; the first that works wins.
        let mut t = now;
        while ((t - now) as f64) < period * 1.5 {
            let mut at = t as f64;
            if jobs.iter().all(|&(a, b, d)| {
                let ok = ok_at(at, a, b, d);
                at += d;
                ok
            }) {
                return (t > now).then_some(t);
            }
            t += 100_000;
        }
        None
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
        self.mark(r);
        self.present_pending();
    }

    /// What the panel scans out right now, in view orientation (RGBA), for debugging.
    pub fn dump_front(&self, out: &mut [u32]) -> bool {
        let r = Rect { x0: 0, y0: 0, x1: self.vw, y1: self.vh };
        let rect = self.buffer_rect(r);
        let mut p: *mut c_void = std::ptr::null_mut();
        unsafe {
            if AHardwareBuffer_lock(self.buf, 2 /* CPU_READ_RARELY */, -1, &rect, &mut p) != 0 || p.is_null() {
                return false;
            }
            let base = p as *const u32;
            for y in 0..self.vh {
                for x in 0..self.vw {
                    let (bx, by) = self.map(x, y);
                    out[y * self.vw + x] = *base.add(by * self.stride + bx);
                }
            }
            AHardwareBuffer_unlock(self.buf, std::ptr::null_mut());
        }
        true
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
            // Far apart: two small presents beat one big box.
            (Some(a), Some(b)) if a.union(b).area() > 2 * (a.area() + b.area()) => {
                self.mark(a);
                self.present_pending();
                self.mark(b);
            }
            (Some(a), Some(b)) => self.mark(a.union(b)),
            (Some(a), None) | (None, Some(a)) => self.mark(a),
            (None, None) => {}
        }
    }
}

impl Drop for Front {
    fn drop(&mut self) {
        unsafe {
            AHardwareBuffer_release(self.buf);
            if let Some(t) = self.twin {
                AHardwareBuffer_release(t);
            }
            for (b, _) in &self.chain {
                AHardwareBuffer_release(*b);
            }
        }
    }
}
