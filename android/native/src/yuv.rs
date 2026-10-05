//! The screen as NV12 (Y plane, then interleaved Cb Cr), handed to the display as YUV.
//!
//! RGBA buffers cost the display controller 4 bytes per pixel to read, 13 MB per frame here:
//! at 90 Hz it fell behind (FIFO underrun) and the end of each panel line, the upper half of
//! the landscape screen, showed gray. The video path the system itself uses reads compressed
//! YUV. NV12 is 1.5 bytes per pixel, the tablet's tiles and decoded frames already are NV12,
//! and the display rotates and converts it itself: updates are plain row copies.

use crate::front::{AHB, AHardwareBuffer_acquire, AHardwareBuffer_fromHardwareBuffer, AHardwareBuffer_lockPlanes,
    AHardwareBuffer_release, AHardwareBuffer_unlock, GUARD_NS, Planes, Rect, SCAN_FRACTION, Yuv, now_ns, wait_until};
use jni_sys::{JNIEnv, jobject};

const CPU_WRITE_RARELY: u64 = 2 << 4;

pub struct Screen {
    w: usize,
    h: usize,
    y: Vec<u8>,
    uv: Vec<u8>, // Cb Cr pairs, w bytes per row, h / 2 rows
    /// Swap-chain buffers and, per buffer, the areas it is missing.
    buffers: Vec<(*mut AHB, Vec<Rect>)>,
    /// Front-buffer mode: the one buffer the panel scans out, written in place.
    front: Option<Front>,
    cursor: Cursor,
}

/// The buffer the display scans out, written while the scan is elsewhere.
struct Front {
    buf: *mut AHB,
    /// Changed since the last present (screen rects).
    pending: Vec<Rect>,
    /// SurfaceControl buffer transform hint: how screen pixels map to the panel's scan.
    hint: i32,
    vsync_ns: i64,
    period_ns: i64,
    /// Writing cost (running average), to plan around the scan.
    ns_per_px: f64,
    /// When the scan passes (shows) the last frame written: the next frame is not written
    /// before, so each pass shows one more frame, never two (smooth motion).
    visible_ns: i64,
    /// Frames shown per scan pass (smoothness log, every 5 s).
    pacing: std::collections::BTreeMap<i64, u32>,
}

/// The computer's cursor, blended into the picture where it is written out (front mode).
#[derive(Default)]
struct Cursor {
    /// Per pixel: Y, Cb, Cr (video range) and alpha.
    pixels: Vec<[u8; 4]>,
    w: usize,
    h: usize,
    x: i64,
    y: i64,
    shown: bool,
    drawn: Option<Rect>,
}

unsafe impl Send for Screen {}

/// Even bounds (chroma covers 2x2 pixels), clipped to the screen.
fn chroma_aligned(r: Rect, w: usize, h: usize) -> Rect {
    Rect { x0: r.x0 & !1, y0: r.y0 & !1, x1: (r.x1.min(w) + 1) & !1, y1: (r.y1.min(h) + 1) & !1 }
}

impl Screen {
    pub fn new(w: usize, h: usize) -> Self {
        // Black in video range: Y 16, Cb Cr 128.
        Self { w, h, y: vec![16; w * h], uv: vec![128; w * h / 2], buffers: Vec::new(), front: None, cursor: Cursor::default() }
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
        self.mark(r);
    }

    fn mark(&mut self, r: Rect) {
        for (_, stale) in &mut self.buffers {
            add_merged(stale, r);
        }
        if let Some(f) = &mut self.front {
            add_merged(&mut f.pending, r);
        }
    }

    /// Front-buffer mode: `hb` (an NV12 buffer of the screen's size) is scanned out directly.
    pub unsafe fn attach_front(&mut self, env: *mut JNIEnv, hb: jobject, hint: i32) -> bool {
        unsafe {
            let b = AHardwareBuffer_fromHardwareBuffer(env, hb);
            if b.is_null() {
                return false;
            }
            AHardwareBuffer_acquire(b);
            let all = Rect { x0: 0, y0: 0, x1: self.w, y1: self.h };
            self.front = Some(Front {
                buf: b,
                pending: vec![all],
                hint,
                vsync_ns: 0,
                period_ns: 0,
                ns_per_px: 0.5,
                visible_ns: 0,
                pacing: Default::default(),
            });
        }
        self.present(false)
    }

    pub fn set_vsync(&mut self, vsync_ns: i64, period_ns: i64) {
        if let Some(f) = &mut self.front {
            f.vsync_ns = vsync_ns;
            f.period_ns = period_ns;
        }
    }

    /// The panel rows (in scan order) screen rectangle `r` occupies, and how many rows there are.
    fn scan_rows(&self, hint: i32, r: Rect) -> (f64, f64, f64) {
        let (w, h) = (self.w as f64, self.h as f64);
        match hint {
            4 => (r.x0 as f64, r.x1 as f64, w),                // rotate 90: panel row = screen column
            7 => (w - r.x1 as f64, w - r.x0 as f64, w),        // rotate 270
            3 => (h - r.y1 as f64, h - r.y0 as f64, h),        // rotate 180
            _ => (r.y0 as f64, r.y1 as f64, h),
        }
    }

    /// Front mode: writes everything changed since the last call into the scanned-out buffer,
    /// timed so the panel's scan is in none of the areas while they are written (no tearing),
    /// with the cursor blended in. Areas far apart stay separate (a few small writes instead of
    /// one box around them), all in the same refresh. `frame`: a new video frame, paced to one
    /// per scan pass (cursor moves are not).
    pub fn present(&mut self, frame: bool) -> bool {
        let Some(f) = &mut self.front else { return true };
        let mut rects = std::mem::take(&mut f.pending);
        if rects.is_empty() {
            return true;
        }
        let (hint, buf, per_px, visible) = (f.hint, f.buf, f.ns_per_px, f.visible_ns);
        rects.sort_by(|a, b| self.scan_rows(hint, *a).0.total_cmp(&self.scan_rows(hint, *b).0));
        let jobs: Vec<(f64, f64, f64, f64)> = rects
            .iter()
            .map(|r| {
                let (a, b, rows) = self.scan_rows(hint, *r);
                (a, b, rows, ((r.x1 - r.x0) * (r.y1 - r.y0)) as f64 * per_px)
            })
            .collect();
        let not_before = if frame { visible } else { 0 };
        if let Some(t) = self.plan(&jobs, not_before) {
            wait_until(t);
        }
        let started = now_ns();
        let ok = self.write_front(buf, &rects);
        if frame
            && let Some(f) = &mut self.front
            && f.period_ns > 0
        {
            // The next time the scan reaches the first row written.
            let period = f.period_ns as f64;
            let row_at = jobs[0].0 / jobs[0].2 * period * SCAN_FRACTION;
            let since = (now_ns() - f.vsync_ns) as f64 - row_at;
            f.visible_ns = now_ns() + (period - since.rem_euclid(period)) as i64;
        }
        let area: usize = rects.iter().map(|r| (r.x1 - r.x0) * (r.y1 - r.y0)).sum();
        if area > 20_000
            && let Some(f) = &mut self.front
        {
            f.ns_per_px = f.ns_per_px * 0.8 + (now_ns() - started) as f64 / area as f64 * 0.2;
        }
        ok
    }

    /// When to start the writes `jobs` (scan rows [a, b) of `rows`, and duration, back to
    /// back): the scan must be in none of them while it is written and not catch up with it.
    fn plan(&self, jobs: &[(f64, f64, f64, f64)], not_before: i64) -> Option<i64> {
        let f = self.front.as_ref()?;
        if f.period_ns <= 0 {
            return None;
        }
        let period = f.period_ns as f64;
        let active = period * SCAN_FRACTION;
        let ok_at = |t: f64, a: f64, b: f64, rows: f64, d: f64| {
            let guard = GUARD_NS / active * rows;
            let (a, b) = ((a - guard).max(0.0), (b + guard).min(rows));
            let phase = (t - f.vsync_ns as f64).rem_euclid(period);
            let scanning = phase < active;
            let beam = phase / active * rows;
            let at_row = |row: f64| row / rows * active;
            let inside = scanning && beam >= a && beam < b;
            let until_b = if scanning && beam < b { at_row(b) - phase } else { period - phase + at_row(b) };
            !inside && until_b >= d + GUARD_NS
        };
        let now = now_ns();
        let start = now.max(not_before);
        let mut t = start;
        while ((t - start) as f64) < period * 1.5 {
            let mut at = t as f64;
            if jobs.iter().all(|&(a, b, rows, d)| {
                let ok = ok_at(at, a, b, rows, d);
                at += d;
                ok
            }) {
                return (t > now).then_some(t);
            }
            t += 50_000;
        }
        None
    }

    fn write_front(&self, buf: *mut AHB, rects: &[Rect]) -> bool {
        unsafe {
            let mut p: Planes = std::mem::zeroed();
            if AHardwareBuffer_lockPlanes(buf, CPU_WRITE_RARELY, -1, std::ptr::null(), &mut p) != 0 || p.count < 3 {
                crate::front::log("front yuv lock failed");
                return false;
            }
            self.copy_rects(&p, rects);
            AHardwareBuffer_unlock(buf, std::ptr::null_mut());
        }
        true
    }

    /// Counts a shown frame towards the smoothness log (front mode).
    pub fn count_frame(&mut self) {
        let Some(f) = &mut self.front else { return };
        if f.period_ns <= 0 {
            return;
        }
        // By the scan pass that shows it.
        let refresh = (f.visible_ns - f.vsync_ns.rem_euclid(f.period_ns)).div_euclid(f.period_ns);
        *f.pacing.entry(refresh).or_default() += 1;
        let first = *f.pacing.keys().next().unwrap();
        if (refresh - first) * f.period_ns >= 5_000_000_000 {
            let (mut zero, mut one, mut more) = (0, 0, 0);
            for r in first..refresh {
                match f.pacing.get(&r).copied().unwrap_or(0) {
                    0 => zero += 1,
                    1 => one += 1,
                    _ => more += 1,
                }
            }
            crate::front::log(&format!("pacing: {one} refreshes with 1 frame, {more} with 2+, {zero} with none"));
            f.pacing.clear();
        }
    }

    /// A new cursor picture: premultiplied RGBA (bytes R, G, B, A), `w` x `h` screen pixels.
    pub fn set_cursor_image(&mut self, rgba: &[u8], w: usize, h: usize) {
        let pixels = rgba
            .chunks_exact(4)
            .map(|p| {
                let a = p[3] as f32;
                // Unpremultiplied colour, then BT.709 video range.
                let un = |c: u8| if a > 0.0 { (c as f32 * 255.0 / a).min(255.0) } else { 0.0 };
                let (r, g, b) = (un(p[0]), un(p[1]), un(p[2]));
                let y = 16.0 + 0.1826 * r + 0.6142 * g + 0.0620 * b;
                let cb = 128.0 - 0.1006 * r - 0.3386 * g + 0.4392 * b;
                let cr = 128.0 + 0.4392 * r - 0.3989 * g - 0.0403 * b;
                [y.round() as u8, cb.round() as u8, cr.round() as u8, p[3]]
            })
            .collect();
        let old = self.cursor_rect();
        self.cursor.pixels = pixels;
        self.cursor.w = w;
        self.cursor.h = h;
        self.redraw_cursor(old);
    }

    /// Moves the cursor's top-left to (x, y) in screen pixels.
    pub fn move_cursor(&mut self, x: i64, y: i64, shown: bool) {
        let old = self.cursor_rect();
        self.cursor.x = x;
        self.cursor.y = y;
        self.cursor.shown = shown;
        self.redraw_cursor(old);
    }

    fn cursor_rect(&self) -> Option<Rect> {
        let c = &self.cursor;
        if !c.shown || c.w == 0 {
            return None;
        }
        let r = Rect {
            x0: c.x.clamp(0, self.w as i64) as usize,
            y0: c.y.clamp(0, self.h as i64) as usize,
            x1: (c.x + c.w as i64).clamp(0, self.w as i64) as usize,
            y1: (c.y + c.h as i64).clamp(0, self.h as i64) as usize,
        };
        (r.x0 < r.x1 && r.y0 < r.y1).then(|| chroma_aligned(r, self.w, self.h))
    }

    fn redraw_cursor(&mut self, old: Option<Rect>) {
        let new = self.cursor_rect();
        self.cursor.drawn = new;
        if let Some(f) = &mut self.front {
            for r in [old, new].into_iter().flatten() {
                add_merged(&mut f.pending, r);
            }
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
            self.copy_rects(&p, &rects);
            AHardwareBuffer_unlock(buf, std::ptr::null_mut());
        }
        true
    }

    /// Copies screen rects from the shadow into locked planes `p`; in front mode the cursor is
    /// blended over them.
    unsafe fn copy_rects(&self, p: &Planes, rects: &[Rect]) {
        unsafe {
            let (y, cb, cr) = (&p.planes[0], &p.planes[1], &p.planes[2]);
            let nv12 = cb.pixel_stride == 2 && cr.data as usize == cb.data as usize + 1;
            let nv21 = cb.pixel_stride == 2 && cb.data as usize == cr.data as usize + 1;
            for &r in rects {
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
                if self.front.is_some() {
                    self.blend_cursor(p, r, nv21);
                }
            }
        }
    }

    /// The cursor over screen rect `r` (already copied), straight into the planes.
    unsafe fn blend_cursor(&self, p: &Planes, r: Rect, nv21: bool) {
        let Some(cr) = self.cursor_rect() else { return };
        let o = Rect { x0: cr.x0.max(r.x0), y0: cr.y0.max(r.y0), x1: cr.x1.min(r.x1), y1: cr.y1.min(r.y1) };
        if o.x0 >= o.x1 || o.y0 >= o.y1 {
            return;
        }
        let c = &self.cursor;
        let (yp, uvp) = (&p.planes[0], if nv21 { &p.planes[2] } else { &p.planes[1] });
        let px = |x: usize, y: usize| {
            let (cx, cy) = (x as i64 - c.x, y as i64 - c.y);
            if cx < 0 || cy < 0 || cx >= c.w as i64 || cy >= c.h as i64 {
                return [0u8; 4];
            }
            c.pixels[cy as usize * c.w + cx as usize]
        };
        let mix = |src: u8, dst: u8, a: u8| ((src as u32 * a as u32 + dst as u32 * (255 - a as u32) + 127) / 255) as u8;
        unsafe {
            for y in o.y0..o.y1 {
                for x in o.x0..o.x1 {
                    let s = px(x, y);
                    if s[3] == 0 {
                        continue;
                    }
                    let d = yp.data.add(y * yp.row_stride as usize + x);
                    *d = mix(s[0], self.y[y * self.w + x], s[3]);
                }
            }
            // Chroma per 2x2 block, from its top-left pixel.
            for cy in (o.y0 / 2)..o.y1.div_ceil(2) {
                for bx in (o.x0 / 2)..o.x1.div_ceil(2) {
                    let s = px(bx * 2, cy * 2);
                    if s[3] == 0 {
                        continue;
                    }
                    let i = cy * self.w + bx * 2;
                    let d = uvp.data.add(cy * uvp.row_stride as usize + bx * 2);
                    let (u, v) = (mix(s[1], self.uv[i], s[3]), mix(s[2], self.uv[i + 1], s[3]));
                    if nv21 {
                        *d = v;
                        *d.add(1) = u;
                    } else {
                        *d = u;
                        *d.add(1) = v;
                    }
                }
            }
        }
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
        if let Some(f) = &self.front {
            unsafe { AHardwareBuffer_release(f.buf) };
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
