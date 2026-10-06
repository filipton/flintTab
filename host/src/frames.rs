//! What happens to each captured frame, on every platform: find what really changed, send
//! small changes as exact pixels (tiles) and the rest through the platform's H.264 encoder,
//! at most as fast as the tablet takes them, and sharpen the last lossy area while idle.
//!
//! A backend captures NV12 frames into [`Frames::push`] and runs [`Frames::run`] on a thread
//! of its own with its encoder.

use std::sync::{Arc, Mutex, mpsc};

use crate::{
    Control,
    gate::{Gate, MAX_REPEATS},
    protocol,
    tiles::{self, Picture, Rect},
    timing::Timing,
};

/// A captured frame's pixels.
pub trait Buffer: Clone + Send + 'static {
    type Pic<'a>: Picture
    where
        Self: 'a;
    /// Read access to the NV12 pixels; `None` if the frame cannot be read as NV12.
    fn picture(&self) -> Option<Self::Pic<'_>>;
}

/// A captured frame and when it was composited / handed to us (session clock, µs).
#[derive(Clone)]
pub struct Frame<B> {
    pub buf: B,
    pub composited: Option<u64>,
    pub delivered: u64,
}

impl<T: Clone + Send> Control for Gate<T> {
    fn ack(&self) {
        Gate::ack(self)
    }
    fn request_keyframe(&self) {
        Gate::request_keyframe(self)
    }
    fn close(&self) {
        Gate::close(self)
    }
}

/// A pixel rectangle as 0..=65535 fractions of the frame, rounded outwards.
pub fn normalize(r: Rect, w: u32, h: u32) -> [u16; 4] {
    let n = |v: f64, size: u32, up: bool| {
        let f = (v / size as f64 * 65535.0).clamp(0.0, 65535.0);
        (if up { f.ceil() } else { f.floor() }) as u16
    };
    [n(r[0], w, false), n(r[1], h, false), n(r[2], w, true), n(r[3], h, true)]
}

/// The part of the screen an H.264 frame is (pixels; x, y even, sizes multiples of 16).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Region {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl Region {
    pub fn wire(&self) -> [u16; 4] {
        [self.x as u16, self.y as u16, self.w as u16, self.h as u16]
    }
    fn contains(&self, r: Rect) -> bool {
        r[0] >= self.x as f64 && r[1] >= self.y as f64 && r[2] <= (self.x + self.w) as f64 && r[3] <= (self.y + self.h) as f64
    }
    fn area(&self) -> u64 {
        self.w as u64 * self.h as u64
    }
    fn clip(&self, r: Rect) -> Rect {
        [
            r[0].max(self.x as f64),
            r[1].max(self.y as f64),
            r[2].min((self.x + self.w) as f64),
            r[3].min((self.y + self.h) as f64),
        ]
    }
    fn overlaps(&self, r: Rect) -> bool {
        r[0] < (self.x + self.w) as f64 && r[2] > self.x as f64 && r[1] < (self.y + self.h) as f64 && r[3] > self.y as f64
    }
}

/// Which part of the screen to encode. A change in part of the screen (a video in a window)
/// is encoded at that part's size: the encoder and the tablet's decoder take time by pixel
/// (a whole 2304x1440 frame: ~7 ms to encode on an M4 Pro, ~9 ms to decode on an Exynos).
/// The part stays put while changes stay inside it (a new size costs a keyframe).
struct Regions {
    width: u32,
    height: u32,
    current: Region,
    /// H.264 frames in a row whose change was small: leaving the whole screen for a part.
    small_streak: u32,
    /// Frames in a row that used little of the current part: time to fit a smaller one.
    loose_streak: u32,
    /// Frames since the last big change: a part with none for a while is given up (small
    /// changes go back to exact tiles).
    since_big: u32,
    /// Any big change at all yet.
    seen_big: bool,
    /// Small frames needed before the next part, after one was outgrown.
    cooldown: u32,
}

/// Frames without a big change after which a part is given up (~0.7 s at 90 Hz).
const PART_IDLE_FRAMES: u32 = 60;

/// Changes up to this many pixels go as tiles beside a part of the screen the codec takes.
const SMALL_CHANGE: f64 = 128.0 * 128.0;

/// A part is started for changes up to this share of the screen (percent), and kept (grown)
/// up to the second: a change near one limit does not flip between part and whole screen.
const REGION_ENTER_PERCENT: u64 = 40;
const REGION_KEEP_PERCENT: u64 = 55;

impl Regions {
    fn new(width: u32, height: u32) -> Self {
        Self { width, height, current: Region { x: 0, y: 0, w: width, h: height }, small_streak: 0, loose_streak: 0, since_big: 0, seen_big: false, cooldown: 0 }
    }

    /// The codec has had big changes lately (a video playing): its part's changes are its own.
    fn active(&self) -> bool {
        self.seen_big && self.since_big < PART_IDLE_FRAMES
    }

    /// A frame with (or without) a big change went by.
    fn note(&mut self, big: bool) {
        if big {
            self.since_big = 0;
            self.seen_big = true;
            return;
        }
        self.since_big += 1;
        if self.since_big >= PART_IDLE_FRAMES {
            self.current = self.full();
            self.small_streak = 0;
        }
    }

    fn full(&self) -> Region {
        Region { x: 0, y: 0, w: self.width, h: self.height }
    }

    /// `r` grown out to the 64-pixel grid, inside the screen: a change whose edge wanders by a
    /// few pixels (a ball crossing a video's border) keeps the same part, not a new size
    /// (and a keyframe) every frame.
    fn around(&self, r: Rect) -> Region {
        let x0 = (r[0].max(0.0) as u32) & !63;
        let y0 = (r[1].max(0.0) as u32) & !63;
        let x1 = ((r[2].ceil() as u32 + 63) & !63).min(self.width);
        let y1 = ((r[3].ceil() as u32 + 63) & !63).min(self.height);
        Region { x: x0, y: y0, w: (x1 - x0).max(16), h: (y1 - y0).max(16) }
    }

    fn fits(&self, r: &Region, percent: u64) -> bool {
        r.area() * 100 <= self.full().area() * percent
    }

    /// The part for a frame whose change is `changed` (`None`: the whole screen).
    fn pick(&mut self, changed: Option<Rect>) -> Region {
        let full = self.full();
        let Some(a) = changed else {
            self.small_streak = 0;
            self.current = full;
            return full;
        };
        let want = self.around(a);
        let limit = if self.current == full { REGION_ENTER_PERCENT } else { REGION_KEEP_PERCENT };
        if !self.fits(&want, limit) {
            self.small_streak = 0;
            self.current = full;
            return full;
        }
        if self.current == full {
            // Several small changes in a row first (~0.2 s): leaving and coming back to the whole
            // screen costs two keyframes, for e.g. a menu between two scrolls, or a frame that
            // changed only part of a video that fills most of the screen.
            self.small_streak += 1;
            if self.small_streak < 20.max(self.cooldown) {
                return full;
            }
            self.current = want;
            self.loose_streak = 0;
            self.cooldown = 0;
            return want;
        }
        let c = self.current;
        if c.contains(a) {
            // Mostly unused for a while (the change moved or shrank): a part that fits it.
            if want.area() * 4 < c.area() {
                self.loose_streak += 1;
                if self.loose_streak >= 30 {
                    self.current = want;
                    self.loose_streak = 0;
                }
            } else {
                self.loose_streak = 0;
            }
            return self.current;
        }
        // Outside the part: grow it to take both, with room to spare (every new size is a
        // keyframe, and a change that crossed the border once tends to wander further) unless
        // that is most of the screen.
        let x0 = c.x.min(want.x);
        let y0 = c.y.min(want.y);
        let x1 = (c.x + c.w).max(want.x + want.w);
        let y1 = (c.y + c.h).max(want.y + want.h);
        let grown = self.around([x0 as f64 - 128.0, y0 as f64 - 128.0, x1 as f64 + 128.0, y1 as f64 + 128.0]);
        let both = Region { x: x0, y: y0, w: x1 - x0, h: y1 - y0 };
        self.current = if self.fits(&grown, REGION_KEEP_PERCENT) {
            grown
        } else if self.fits(&both, REGION_KEEP_PERCENT) {
            both
        } else {
            // Outgrown: the whole screen, and for a while (no part again after a few quiet
            // frames, only to outgrow it again: two keyframes each time).
            self.small_streak = 0;
            self.cooldown = 90;
            full
        };
        self.loose_streak = 0;
        self.current
    }
}

pub struct Frames<B> {
    pub gate: Arc<Gate<Frame<B>>>,
    /// Area changed since the last frame sent (pixels), including frames flow control skipped.
    changed: Mutex<Vec<Rect>>,
    timing: Arc<Timing>,
    width: u32,
    height: u32,
}

impl<B: Buffer> Frames<B> {
    pub fn new(timing: Arc<Timing>, width: u32, height: u32) -> Arc<Self> {
        Arc::new(Self { gate: Arc::new(Gate::new()), changed: Mutex::new(Vec::new()), timing, width, height })
    }

    /// A captured frame: `composited` when the system drew it (session clock, µs), `dirty`
    /// the areas it says changed (pixels; `None`: it does not say, the whole frame is checked).
    pub fn push(&self, buf: B, composited: Option<u64>, dirty: Option<Vec<Rect>>) {
        {
            let mut c = self.changed.lock().unwrap();
            for r in dirty.unwrap_or_else(|| vec![[0.0, 0.0, self.width as f64, self.height as f64]]) {
                tiles::add(&mut c, r);
            }
        }
        let delivered = self.timing.now();
        self.timing.captured();
        self.gate.push(Frame { buf, composited, delivered });
    }

    /// Hands frames out until the session ends: tiles go straight to `tx`, everything else to
    /// `encode(frame, pts, changed area, keyframe)`. The backend's encoder output calls
    /// [`Frames::encoded`] with the same pts and area.
    /// `regions`: the encoder can take a part of the screen (`encode` gets the part to encode;
    /// else always the whole screen).
    pub fn run(
        &self,
        use_tiles: bool,
        regions: bool,
        tx: mpsc::Sender<Vec<u8>>,
        mut encode: impl FnMut(&Frame<B>, u64, [u16; 4], Region, bool),
    ) {
        let mut parts = Regions::new(self.width, self.height);
        let debug_regions = std::env::var_os("TD_DEBUG_REGIONS").is_some();
        let mut last_size = None;
        let (gate, timing) = (&self.gate, &self.timing);
        let mut first = true;
        let mut shadow = tiles::Shadow::new();
        // Areas last sent through H.264 since the idle repeats last ran: only those need
        // re-sharpening (tiles are exact), and none at all after pure typing.
        let mut lossy: Option<Rect> = None;
        // Tiles went out since the last H.264 frame: the encoder's reference no longer
        // matches the screen, and a P-frame copying "unchanged" blocks from it would
        // put stale pictures (old text, old bar positions) on the tablet.
        let mut stale_reference = false;
        let mut repeats = 0;
        #[cfg(target_os = "macos")]
        unsafe {
            libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE, 0);
        }
        while let Some(job) = gate.next() {
            let f = &job.frame;
            let reported = std::mem::take(&mut *self.changed.lock().unwrap());
            let pic = f.buf.picture();
            // What really changed inside what was reported (repeats re-send the same frame).
            let reported_n = reported.len();
            let reported_b = tiles::bounds(&reported);
            let mut refined = None;
            let rects = match &pic {
                Some(p) if !job.repeat => match shadow.changed(p, &reported) {
                    Some(r) => {
                        refined = Some(r.len());
                        r
                    }
                    None => reported,
                },
                _ => reported,
            };
            if debug_regions && !job.repeat {
                eprintln!("regions: reported {reported_n} rects in {:?}, diff {refined:?}, pic {}", reported_b.map(|r| r.map(|v| v as i32)), pic.is_some());
            }
            let area = tiles::bounds(&rects);
            let full = job.keyframe || first;
            // The frame's time (its tiles' are just after), and its part if already chosen.
            let mut now_frame = 0;
            let mut picked = None;
            let rect = if full {
                None
            } else if job.repeat {
                repeats += 1;
                let r = lossy;
                if repeats >= MAX_REPEATS {
                    lossy = None;
                }
                let Some(r) = r else { continue };
                Some(r)
            } else {
                repeats = 0;
                // Nothing changed at all (e.g. only the cursor, which is not in the video).
                let Some(a) = area else { continue };
                let now = timing.now();
                now_frame = now;
                let big = tiles::bounds(&rects.iter().copied().filter(|r| tiles::size(*r) > SMALL_CHANGE).collect::<Vec<_>>());
                // While a part of the screen is the codec's (a video playing), changes in it go
                // through the codec too: its last picture stays what the tablet shows, so a clock
                // ticking inside a video's window costs a small P-frame, not a keyframe.
                // The same while the whole screen is the codec's (a video near full screen): a tile
                // would make its last picture stale and the next frame a keyframe.
                let in_part = parts.active() && rects.iter().any(|r| parts.current.overlaps(*r));
                // Small change: the exact pixels, no codec.
                if let Some(msgs) =
                    pic.as_ref().filter(|_| use_tiles && !in_part).and_then(|p| tiles::build_all(p, &rects, now))
                {
                    parts.note(false);
                    if debug_regions {
                        eprintln!("regions: tiles {:?} (active {}, part {:?})", rects.iter().map(|r| r.map(|v| v as i32)).collect::<Vec<_>>(), parts.active(), parts.current);
                    }
                    gate.sent_batch(msgs.len());
                    // The encoder's last picture is out of date where the tiles went.
                    if !regions || parts.current == parts.full() {
                        stale_reference = true;
                    }
                    // One send: the writer puts the frame's tiles into one USB transfer.
                    let mut all = Vec::with_capacity(msgs.iter().map(Vec::len).sum());
                    for (i, msg) in msgs.into_iter().enumerate() {
                        let pts = now + i as u64;
                        timing.encode_started(pts, f.composited, Some(f.delivered), false);
                        timing.encoded(pts, msg.len());
                        all.extend_from_slice(&msg);
                    }
                    tx.send(all).ok();
                    continue;
                }
                // Too much for tiles (a video playing): the codec is busy for a while. (Changes it
                // was only given because it is busy do not keep it so.)
                parts.note(!in_part || big.is_some());
                // Big changes (a video playing) through the codec, in a part of the screen around
                // them; small ones elsewhere (a clock, a menu) as tiles, sent first.
                let mut a = a;
                if regions
                    && use_tiles
                    && let Some(p) = pic.as_ref()
                    && (big.is_some() || (in_part && parts.current != parts.full()))
                {
                    let region = match big {
                        Some(b) => parts.pick(Some(b)),
                        None => parts.current,
                    };
                    // Partly inside: as a tile (exact), and its inside part through the codec too.
                    let (inside, outside): (Vec<Rect>, Vec<Rect>) = rects.iter().partition(|r| region.contains(**r));
                    let touched = tiles::bounds(&rects.iter().copied().filter(|r| region.overlaps(*r)).collect::<Vec<_>>())
                        .map(|t| region.clip(t));
                    let tiles = (region != parts.full()).then(|| {
                        let mut bytes = 0;
                        outside
                            .iter()
                            .enumerate()
                            .map(|(i, r)| {
                                let m = tiles::build(p, *r, now + 1 + i as u64)?;
                                bytes += m.len();
                                (bytes <= tiles::MAX_BYTES).then_some(m)
                            })
                            .collect::<Option<Vec<_>>>()
                    });
                    if region == parts.full() {
                        picked = Some(region);
                    }
                    if let Some(Some(msgs)) = tiles {
                        if !msgs.is_empty() {
                            gate.sent_batch(msgs.len());
                            let mut all = Vec::with_capacity(msgs.iter().map(Vec::len).sum());
                            for (i, msg) in msgs.into_iter().enumerate() {
                                let pts = now + 1 + i as u64;
                                timing.encode_started(pts, f.composited, Some(f.delivered), false);
                                timing.encoded(pts, msg.len());
                                all.extend_from_slice(&msg);
                            }
                            tx.send(all).ok();
                        }
                        a = touched.or(tiles::bounds(&inside)).or(big).unwrap_or(a);
                        picked = Some(region);
                    }
                }
                lossy = Some(lossy.map_or(a, |l| [l[0].min(a[0]), l[1].min(a[1]), l[2].max(a[2]), l[3].max(a[3])]));
                Some(a)
            };
            drop(pic);
            first = false;
            let now = if job.repeat || full { timing.now() } else { now_frame };
            timing.encode_started(now, f.composited, Some(f.delivered), job.repeat);
            let region = if !regions { parts.full() } else { picked.unwrap_or_else(|| parts.pick(rect)) };
            if debug_regions {
                eprintln!("regions: {} rects {:?} -> part {region:?}", rects.len(), rects.iter().map(|r| r.map(|v| v as i32)).collect::<Vec<_>>());
            }
            // The first frame too: an encoder may have seen frames before (a test frame). A
            // part of another size starts over (its encoder's last frame is not the tablet's).
            let resized = last_size.replace((region.w, region.h)) != Some((region.w, region.h));
            let stale = std::mem::take(&mut stale_reference);
            let keyframe = full || resized || stale;
            if keyframe {
                timing.keyframe();
                if debug_regions {
                    eprintln!("regions: keyframe (requested {}, new size {resized}, stale {stale})", job.keyframe);
                }
            }
            encode(f, now, rect.map_or(protocol::ALL, |r| normalize(r, self.width, self.height)), region, keyframe);
        }
    }

    /// The whole screen as a region.
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    pub fn whole(&self) -> Region {
        Region { x: 0, y: 0, w: self.width, h: self.height }
    }

    /// The encoder finished the frame encoded as `pts`: on its way to the tablet.
    pub fn encoded(&self, tx: &mpsc::Sender<Vec<u8>>, pts: u64, area: [u16; 4], region: Region, au: &[u8]) {
        self.gate.sent();
        self.timing.video(pts);
        self.timing.encoded(pts, au.len());
        tx.send(protocol::video_msg(pts, area, region.wire(), au)).ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_video_gets_a_part_after_a_few_frames_and_keeps_it() {
        let mut p = Regions::new(2304, 1440);
        let video = [512.0, 360.0, 1792.0, 1080.0];
        for _ in 0..19 {
            assert_eq!(p.pick(Some(video)), p.full());
        }
        let part = p.pick(Some(video));
        assert_eq!(part, Region { x: 512, y: 320, w: 1280, h: 768 });
        // A ball crossing the border: the part grows with room to spare, and then stays.
        let grown = Region { x: 320, y: 192, w: 1664, h: 1024 };
        assert_eq!(p.pick(Some([508.0, 360.0, 1796.0, 1080.0])), grown);
        assert_eq!(p.pick(Some([500.0, 350.0, 1800.0, 1090.0])), grown);
        assert_eq!(p.pick(Some(video)), grown);
    }

    #[test]
    fn big_changes_take_the_whole_screen() {
        let mut p = Regions::new(2304, 1440);
        for _ in 0..20 {
            p.pick(Some([512.0, 360.0, 1792.0, 1080.0]));
        }
        assert_eq!(p.pick(Some([0.0, 0.0, 2000.0, 1200.0])), p.full());
        assert_eq!(p.pick(None), p.full());
    }
}
