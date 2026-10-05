//! Small screen updates as compressed pixels instead of through the video codec.
//!
//! H.264 costs ~7 ms to encode here and ~9 ms to decode on the tablet, whatever the size of
//! the change. Typing, a blinking caret, a menu or a small window only touch a few thousand
//! pixels: those go out as the exact NV12 pixels of the changed area, LZ4-compressed (~1 ms),
//! and the tablet draws them straight away. Bigger changes still go through H.264, which needs
//! far fewer bytes than the USB link could move in the same time.

use screencapturekit::CVPixelBuffer;

use crate::protocol;

/// Changed areas kept apart (the menu bar clock and a window far below it are two tiles, not
/// one box over half the screen); more get merged.
const MAX_RECTS: usize = 8;

pub type Rect = [f64; 4];

fn union(a: Rect, b: Rect) -> Rect {
    [a[0].min(b[0]), a[1].min(b[1]), a[2].max(b[2]), a[3].max(b[3])]
}

fn size(r: Rect) -> f64 {
    (r[2] - r[0]).max(0.0) * (r[3] - r[1]).max(0.0)
}

pub fn bounds(rects: &[Rect]) -> Option<Rect> {
    rects.iter().copied().reduce(union)
}

/// Adds `r` to `rects`, merging it with a rectangle it overlaps or nearly touches, and keeping
/// at most MAX_RECTS by merging the pair whose union wastes the least area.
pub fn add(rects: &mut Vec<Rect>, r: Rect) {
    if size(r) <= 0.0 {
        return;
    }
    let mut r = r;
    // Absorb neighbours (repeatedly, a merged box may reach further ones).
    loop {
        let near = rects.iter().position(|o| {
            let u = union(*o, r);
            size(u) <= (size(*o) + size(r)) * 1.25 + 64.0 * 64.0
        });
        match near {
            Some(i) => r = union(rects.swap_remove(i), r),
            None => break,
        }
    }
    rects.push(r);
    while rects.len() > MAX_RECTS {
        let mut best = (0, 1, f64::MAX);
        for i in 0..rects.len() {
            for j in i + 1..rects.len() {
                let waste = size(union(rects[i], rects[j])) - size(rects[i]) - size(rects[j]);
                if waste < best.2 {
                    best = (i, j, waste);
                }
            }
        }
        let b = rects.swap_remove(best.1);
        rects[best.0] = union(rects[best.0], b);
    }
}

/// The last frame sent, to find which pixels really changed: ScreenCaptureKit reports whole
/// windows as changed even when only a caret blinked in them.
pub struct Shadow {
    y: Vec<u8>,
    uv: Vec<u8>,
    w: usize,
    h: usize,
}

const BLOCK: usize = 16;

impl Shadow {
    pub fn new() -> Self {
        Self { y: Vec::new(), uv: Vec::new(), w: 0, h: 0 }
    }

    /// The 16x16 blocks inside `rects` that differ from the last frame (merged into a few
    /// rectangles), remembering this frame's pixels there. `None` if the frame is not NV12;
    /// everything counts as changed on the first frame or after a size change.
    pub fn changed(&mut self, buf: &CVPixelBuffer, rects: &[Rect]) -> Option<Vec<Rect>> {
        let lock = buf.lock_read_only().ok()?;
        if lock.plane_count() != 2 {
            return None;
        }
        let (w, h) = (lock.width_of_plane(0), lock.height_of_plane(0));
        let fresh = (w, h) != (self.w, self.h);
        if fresh {
            self.y = vec![0; w * h];
            self.uv = vec![0; w * h / 2];
            self.w = w;
            self.h = h;
        }
        let whole = [0.0, 0.0, w as f64, h as f64];
        let scan: Vec<Rect> = if fresh { vec![whole] } else { rects.to_vec() };
        let mut out = Vec::new();
        for r in scan {
            let bx0 = (r[0].max(0.0) as usize / BLOCK) * BLOCK;
            let by0 = (r[1].max(0.0) as usize / BLOCK) * BLOCK;
            let bx1 = ((r[2].ceil().max(0.0) as usize).div_ceil(BLOCK) * BLOCK).min(w);
            let by1 = ((r[3].ceil().max(0.0) as usize).div_ceil(BLOCK) * BLOCK).min(h);
            let mut by = by0;
            while by < by1 {
                let bh = BLOCK.min(by1 - by);
                // One flag per block column in this block row.
                let mut diff = vec![fresh; (bx1 - bx0).div_ceil(BLOCK)];
                for row in by..by + bh {
                    let src = lock.plane_row(0, row)?;
                    let dst = &mut self.y[row * w..row * w + w];
                    for (i, d) in diff.iter_mut().enumerate() {
                        let x = bx0 + i * BLOCK;
                        let x1 = (x + BLOCK).min(bx1);
                        if src[x..x1] != dst[x..x1] {
                            *d = true;
                            dst[x..x1].copy_from_slice(&src[x..x1]);
                        }
                    }
                }
                for row in by / 2..(by + bh) / 2 {
                    let src = lock.plane_row(1, row)?;
                    let dst = &mut self.uv[row * w..row * w + w];
                    for (i, d) in diff.iter_mut().enumerate() {
                        let x = bx0 + i * BLOCK;
                        let x1 = (x + BLOCK).min(bx1);
                        if src[x..x1] != dst[x..x1] {
                            *d = true;
                            dst[x..x1].copy_from_slice(&src[x..x1]);
                        }
                    }
                }
                // Runs of changed blocks in this row become rectangles.
                let mut i = 0;
                while i < diff.len() {
                    if diff[i] {
                        let start = i;
                        while i < diff.len() && diff[i] {
                            i += 1;
                        }
                        let x0 = bx0 + start * BLOCK;
                        let x1 = (bx0 + i * BLOCK).min(bx1);
                        add(&mut out, [x0 as f64, by as f64, x1 as f64, (by + bh) as f64]);
                    } else {
                        i += 1;
                    }
                }
                by += bh;
            }
        }
        Some(out)
    }
}

/// Above this share of the screen the codec wins: too many bytes for the link.
pub const MAX_AREA: f64 = 0.4;
/// Compressed tiles larger than this go through H.264 instead (~5 ms over a USB 2 adb link).
pub const MAX_BYTES: usize = 192 * 1024;

/// All of `rects` as tiles (pts `pts`, `pts + 1`, ...), or `None` if the codec should take
/// this frame instead: too large an area, or too many bytes in total.
pub fn build_all(buf: &CVPixelBuffer, rects: &[Rect], pts: u64) -> Option<Vec<Vec<u8>>> {
    let (fw, fh) = (buf.width() as f64, buf.height() as f64);
    if rects.iter().map(|r| size(*r)).sum::<f64>() > MAX_AREA * fw * fh {
        return None;
    }
    let mut out = Vec::with_capacity(rects.len());
    let mut bytes = 0;
    for (i, r) in rects.iter().enumerate() {
        let m = build(buf, *r, pts + i as u64)?;
        bytes += m.len();
        if bytes > MAX_BYTES {
            return None;
        }
        out.push(m);
    }
    Some(out)
}

/// The changed area `r` (x0, y0, x1, y1 in pixels) of an NV12 frame as a MSG_TILE, or `None`
/// if it compresses too poorly to beat the codec. Widened to even coordinates (4:2:0 chroma).
pub fn build(buf: &CVPixelBuffer, r: Rect, pts: u64) -> Option<Vec<u8>> {
    let lock = buf.lock_read_only().ok()?;
    if lock.plane_count() != 2 {
        return None;
    }
    let (fw, fh) = (lock.width_of_plane(0), lock.height_of_plane(0));
    let even_down = |v: f64, max: usize| ((v.max(0.0) as usize) & !1).min(max);
    let even_up = |v: f64, max: usize| ((v.ceil().max(0.0) as usize + 1) & !1).min(max);
    let (x0, y0) = (even_down(r[0], fw), even_down(r[1], fh));
    let (x1, y1) = (even_up(r[2], fw), even_up(r[3], fh));
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    let (w, h) = (x1 - x0, y1 - y0);
    let mut y = Vec::with_capacity(w * h);
    for row in y0..y1 {
        y.extend_from_slice(&lock.plane_row(0, row)?[x0..x1]);
    }
    let mut uv = Vec::with_capacity(w * h / 2);
    for row in y0 / 2..y1 / 2 {
        uv.extend_from_slice(&lock.plane_row(1, row)?[x0..x1]); // interleaved Cb Cr, 2 bytes per 2 pixels
    }
    drop(lock);
    let yc = lz4_flex::block::compress(&y);
    let uvc = lz4_flex::block::compress(&uv);
    if yc.len() + uvc.len() > MAX_BYTES {
        return None;
    }
    Some(protocol::tile_msg(pts, [x0 as u16, y0 as u16, w as u16, h as u16], &yc, &uvc))
}

#[cfg(test)]
mod tests {
    /// Writes LZ4 test vectors for the tablet's decoder test (android/app/src/test).
    #[test]
    fn lz4_vectors() {
        let mut inputs: Vec<Vec<u8>> = vec![
            vec![],
            b"a".to_vec(),
            b"abcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabc".to_vec(),
            vec![16u8; 5000],
        ];
        // UI-like: flat runs, short patterns, some noise
        let mut x: u32 = 1;
        let mut v = Vec::new();
        for i in 0..100_000u32 {
            x = x.wrapping_mul(1103515245).wrapping_add(12345);
            v.push(if i % 700 < 500 { 235 } else if i % 13 == 0 { (x >> 16) as u8 } else { (i % 7) as u8 * 30 });
        }
        inputs.push(v);
        let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
        let out: Vec<String> = inputs.iter().map(|i| format!("{} {}", hex(i), hex(&lz4_flex::block::compress(i)))).collect();
        if let Ok(p) = std::env::var("LZ4_VECTORS") {
            std::fs::write(p, out.join("\n")).unwrap();
        }
        for i in &inputs {
            assert_eq!(&lz4_flex::block::decompress(&lz4_flex::block::compress(i), i.len()).unwrap(), i);
        }
    }
}
