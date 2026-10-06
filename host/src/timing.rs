//! Where a video frame's latency goes, stage by stage, from the moment the computer composited
//! it until the tablet's panel showed it.
//!
//! The host stamps its own stages here. The tablet sends its timestamps back (KIND_TIMING) on
//! its own clock; a ping/pong over the same connection (MSG_PING / KIND_PONG) estimates the
//! offset between the two clocks, NTP-style, from the exchange with the shortest round trip.
//! Every 5 s the median and p95 of each stage are printed.

use std::{
    collections::VecDeque,
    sync::Mutex,
    time::{Duration, Instant},
};

const KEEP_FRAMES: usize = 512;
const KEEP_PONGS: usize = 100; // ~10 s at one ping every 100 ms
const REPORT_EVERY: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Default)]
struct HostFrame {
    pts: u64,
    /// When the compositor finished the frame (macOS: ScreenCaptureKit's display time).
    composited: Option<u64>,
    /// When capture handed it to us.
    delivered: Option<u64>,
    encoded: Option<u64>,
    written: Option<u64>,
    bytes: usize,
    /// An idle re-encode of an older frame: its capture stages are meaningless.
    repeat: bool,
    /// Went through the codec (not a tile).
    video: bool,
}

/// Tablet timestamps of one frame, on the tablet's clock (µs).
pub struct TabletTimes {
    pub recv_start: u64,
    pub recv_end: u64,
    pub queued: u64,
    pub decoded: u64,
    pub shown: u64,
}

const STAGES: [&str; 9] =
    ["capture", "wait", "encode", "send", "usb", "dec-in", "decode", "display", "total"];

#[derive(Default)]
struct Window {
    since: Option<Instant>,
    stages: [Vec<f64>; 9],
    /// The same for video frames alone (tiles take a few ms; these are what can stutter).
    video: [Vec<f64>; 9],
    recv: Vec<f64>,
    bytes: usize,
    frames: usize,
    /// Distinct captured frames shown (a frame's tiles are separate messages).
    shown: usize,
    last_delivered: Option<u64>,
}

struct Inner {
    frames: VecDeque<HostFrame>,
    pongs: VecDeque<(u64, i64)>, // (round trip, tablet clock - host clock), µs
    window: Window,
}

pub struct Timing {
    epoch: Instant,
    inner: Mutex<Inner>,
    captured: std::sync::atomic::AtomicUsize,
    keyframes: std::sync::atomic::AtomicUsize,
}

impl Timing {
    pub fn new(epoch: Instant) -> Self {
        Self {
            epoch,
            inner: Mutex::new(Inner { frames: VecDeque::new(), pongs: VecDeque::new(), window: Window::default() }),
            captured: Default::default(),
            keyframes: Default::default(),
        }
    }

    /// Microseconds on the session clock.
    pub fn now(&self) -> u64 {
        self.epoch.elapsed().as_micros() as u64
    }

    /// The capture delivered a new frame.
    pub fn captured(&self) {
        self.captured.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Frame `pts` went through the codec.
    pub fn video(&self, pts: u64) {
        self.frame(pts, |f| f.video = true);
    }

    /// The encoder was asked for a keyframe (a full picture: big, slow to encode and decode).
    pub fn keyframe(&self) {
        self.keyframes.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Converts a time `age` before now to the session clock.
    pub fn ago(&self, age: Duration) -> u64 {
        self.now().saturating_sub(age.as_micros() as u64)
    }

    fn frame(&self, pts: u64, f: impl FnOnce(&mut HostFrame)) {
        let mut g = self.inner.lock().unwrap();
        if let Some(fr) = g.frames.iter_mut().rev().find(|fr| fr.pts == pts) {
            f(fr);
        }
    }

    /// The encoder starts on a frame; `pts` (encode start, session clock) identifies it from now on.
    pub fn encode_started(&self, pts: u64, composited: Option<u64>, delivered: Option<u64>, repeat: bool) {
        let mut g = self.inner.lock().unwrap();
        if g.frames.len() >= KEEP_FRAMES {
            g.frames.pop_front();
        }
        g.frames.push_back(HostFrame { pts, composited, delivered, repeat, ..Default::default() });
    }

    pub fn encoded(&self, pts: u64, bytes: usize) {
        let now = self.now();
        self.frame(pts, |f| {
            f.encoded = Some(now);
            f.bytes = bytes;
        });
    }

    /// The frame's last byte went into the socket. Backends that do not stamp the earlier
    /// stages (Linux) get a record here, with `pts` as their capture time.
    pub fn written(&self, pts: u64, bytes: usize) {
        let now = self.now();
        let mut g = self.inner.lock().unwrap();
        match g.frames.iter_mut().rev().find(|fr| fr.pts == pts) {
            Some(f) => f.written = Some(now),
            None => {
                if g.frames.len() >= KEEP_FRAMES {
                    g.frames.pop_front();
                }
                g.frames.push_back(HostFrame { pts, composited: Some(pts), written: Some(now), bytes, ..Default::default() });
            }
        }
    }

    /// Answer to a ping sent at `sent` (session clock), received by the tablet at `tablet` (its clock).
    pub fn pong(&self, sent: u64, tablet: u64) {
        let now = self.now();
        // An answer to an earlier session's ping (another clock): ignore it.
        let Some(rtt) = now.checked_sub(sent).filter(|&r| r < 1_000_000) else { return };
        let offset = tablet as i64 - ((sent + now) / 2) as i64;
        let mut g = self.inner.lock().unwrap();
        if g.pongs.len() >= KEEP_PONGS {
            g.pongs.pop_front();
        }
        g.pongs.push_back((rtt, offset));
    }

    /// The tablet showed frame `pts`.
    pub fn tablet(&self, pts: u64, t: TabletTimes) {
        let mut g = self.inner.lock().unwrap();
        let Some(&(rtt, offset)) = g.pongs.iter().min_by_key(|p| p.0) else { return };
        let Some(f) = g.frames.iter().rev().find(|fr| fr.pts == pts).copied() else { return };
        let host = |v: u64| v as i64 - offset;
        let ms = |a: Option<i64>, b: Option<i64>| match (a, b) {
            (Some(a), Some(b)) => Some((b - a) as f64 / 1000.0),
            _ => None,
        };
        let some = |v: Option<u64>| v.map(|v| v as i64);
        let (composited, delivered) = if f.repeat { (None, None) } else { (some(f.composited), some(f.delivered)) };
        let start = some(Some(pts));
        let recv_end = Some(host(t.recv_end));
        let (queued, decoded, shown) = (Some(host(t.queued)), Some(host(t.decoded)), Some(host(t.shown)));
        let values = [
            ms(composited, delivered),
            ms(delivered, start),
            ms(start, some(f.encoded)),
            ms(some(f.encoded), some(f.written)),
            ms(some(f.written), recv_end),
            ms(recv_end, queued),
            ms(queued, decoded),
            ms(decoded, shown),
            ms(composited, shown),
        ];

        let w = &mut g.window;
        let since = *w.since.get_or_insert_with(Instant::now);
        for (v, out) in values.iter().zip(w.stages.iter_mut()) {
            if let Some(v) = v {
                out.push(*v);
            }
        }
        if f.video && !f.repeat {
            for (v, out) in values.iter().zip(w.video.iter_mut()) {
                if let Some(v) = v {
                    out.push(*v);
                }
            }
        }
        w.recv.push((t.recv_end - t.recv_start.min(t.recv_end)) as f64 / 1000.0);
        w.bytes += f.bytes;
        w.frames += 1;
        if f.delivered.is_some() && f.delivered != w.last_delivered {
            w.shown += 1;
            w.last_delivered = f.delivered;
        }
        if since.elapsed() >= REPORT_EVERY {
            let secs = since.elapsed().as_secs_f64();
            let mut line = String::from("latency ms (median/p95):");
            for (name, v) in STAGES.iter().zip(w.stages.iter_mut()) {
                if let Some((m, p)) = median_p95(v) {
                    line += &format!(" {name} {m:.1}/{p:.1}");
                }
            }
            if let Some((m, p)) = median_p95(&mut w.recv) {
                line += &format!(" | on the wire {m:.1}/{p:.1}");
            }
            let captured = self.captured.swap(0, std::sync::atomic::Ordering::Relaxed);
            line += &format!(" | captured {:.0} fps, shown {:.0}", captured as f64 / secs, g.window.shown as f64 / secs);
            let mut rtts: Vec<f64> = g.pongs.iter().map(|p| p.0 as f64 / 1000.0).collect();
            let w = &mut g.window;
            let rtt_median = median_p95(&mut rtts).map_or(0.0, |(m, _)| m);
            line += &format!(
                " | {:.0} fps, {:.0} KB/frame, rtt min {:.2} median {:.2}",
                w.frames as f64 / secs,
                w.bytes as f64 / w.frames as f64 / 1024.0,
                rtt as f64 / 1000.0,
                rtt_median
            );
            let w = &mut g.window;
            if w.video[8].len() >= 5 {
                line += "\n  video frames:";
                for (name, v) in STAGES.iter().zip(w.video.iter_mut()).skip(2) {
                    if let Some((m, p)) = median_p95(v) {
                        line += &format!(" {name} {m:.1}/{p:.1}");
                    }
                }
                line += &format!(" ({} frames)", w.video[8].len());
            }
            let keyframes = self.keyframes.swap(0, std::sync::atomic::Ordering::Relaxed);
            if keyframes > 0 {
                line += &format!(", {keyframes} keyframes");
            }
            println!("{line}");
            *w = Window::default();
        }
    }
}

fn median_p95(v: &mut [f64]) -> Option<(f64, f64)> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(|a, b| a.total_cmp(b));
    let n = v.len();
    Some((v[n / 2], v[(n * 95 / 100).min(n - 1)]))
}
