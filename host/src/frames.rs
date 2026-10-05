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
    pub fn run(&self, use_tiles: bool, tx: mpsc::Sender<Vec<u8>>, mut encode: impl FnMut(&Frame<B>, u64, [u16; 4], bool)) {
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
            let rects = match &pic {
                Some(p) if !job.repeat => shadow.changed(p, &reported).unwrap_or(reported),
                _ => reported,
            };
            let area = tiles::bounds(&rects);
            let full = job.keyframe || first;
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
                // Small change: the exact pixels, no codec.
                let now = timing.now();
                if let Some(msgs) = pic.as_ref().filter(|_| use_tiles).and_then(|p| tiles::build_all(p, &rects, now)) {
                    gate.sent_batch(msgs.len());
                    stale_reference = true;
                    for (i, msg) in msgs.into_iter().enumerate() {
                        let pts = now + i as u64;
                        timing.encode_started(pts, f.composited, Some(f.delivered), false);
                        timing.encoded(pts, msg.len());
                        tx.send(msg).ok();
                    }
                    continue;
                }
                lossy = Some(lossy.map_or(a, |l| [l[0].min(a[0]), l[1].min(a[1]), l[2].max(a[2]), l[3].max(a[3])]));
                Some(a)
            };
            drop(pic);
            first = false;
            let now = timing.now();
            timing.encode_started(now, f.composited, Some(f.delivered), job.repeat);
            // The first frame too: an encoder may have seen frames before (a test frame).
            let keyframe = full || std::mem::take(&mut stale_reference);
            encode(f, now, rect.map_or(protocol::ALL, |r| normalize(r, self.width, self.height)), keyframe);
        }
    }

    /// The encoder finished the frame encoded as `pts`: on its way to the tablet.
    pub fn encoded(&self, tx: &mpsc::Sender<Vec<u8>>, pts: u64, area: [u16; 4], au: &[u8]) {
        self.gate.sent();
        self.timing.encoded(pts, au.len());
        tx.send(protocol::video_msg(pts, area, au)).ok();
    }
}
