//! Decides which captured frame the encoder works on next, and when.
//!
//! - Newest frame wins: capture only replaces a single slot, so the encoder never works
//!   through a backlog and the capture callback never blocks on the encoder.
//! - Flow control: at most `MAX_IN_FLIGHT` encoded frames may be unacknowledged by the
//!   tablet. adb relays the socket through two daemons with their own buffers, so the
//!   host's socket queue alone says little about how far behind the tablet is.
//! - Idle refresh: ScreenCaptureKit only delivers frames when something changes, so the
//!   last frame after a scroll would keep whatever (low) quality the rate control gave it.
//!   Re-encoding it a few times while idle lets the encoder sharpen it, and also resends
//!   the final state if that frame was skipped by flow control (scrcpy does the same with
//!   `KEY_REPEAT_PREVIOUS_FRAME_AFTER`).

use std::{
    sync::{Condvar, Mutex},
    time::{Duration, Instant},
};

pub const MAX_IN_FLIGHT: usize = 2;
pub const REPEAT_AFTER: Duration = Duration::from_millis(100);
pub const MAX_REPEATS: u32 = 8;

struct State<T> {
    latest: Option<T>,
    fresh: bool,
    want_key: bool,
    in_flight: usize,
    closed: bool,
    last_encode: Instant,
    repeats: u32,
}

pub struct Gate<T> {
    state: Mutex<State<T>>,
    cv: Condvar,
}

pub struct Job<T> {
    pub frame: T,
    pub keyframe: bool,
}

impl<T: Clone> Gate<T> {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(State {
                latest: None,
                fresh: false,
                want_key: false,
                in_flight: 0,
                closed: false,
                last_encode: Instant::now(),
                repeats: MAX_REPEATS,
            }),
            cv: Condvar::new(),
        }
    }

    fn update(&self, f: impl FnOnce(&mut State<T>)) {
        f(&mut self.state.lock().unwrap());
        self.cv.notify_all();
    }

    /// A new captured frame; replaces any frame the encoder has not picked up yet.
    pub fn push(&self, frame: T) {
        self.update(|s| {
            s.latest = Some(frame);
            s.fresh = true;
        });
    }

    /// The encoder produced a frame that is now on its way to the tablet.
    pub fn sent(&self) {
        self.update(|s| s.in_flight += 1);
    }

    /// The tablet took a frame off the wire.
    pub fn ack(&self) {
        self.update(|s| s.in_flight = s.in_flight.saturating_sub(1));
    }

    /// The tablet's decoder restarted and needs a keyframe.
    pub fn request_keyframe(&self) {
        self.update(|s| s.want_key = true);
    }

    pub fn close(&self) {
        self.update(|s| s.closed = true);
    }

    /// Blocks until there is something to encode; `None` once closed.
    pub fn next(&self) -> Option<Job<T>> {
        let mut s = self.state.lock().unwrap();
        loop {
            if s.closed {
                return None;
            }
            let now = Instant::now();
            let repeat_at = s.last_encode + REPEAT_AFTER;
            let repeat_due = s.repeats < MAX_REPEATS && now >= repeat_at;
            if s.latest.is_some() && s.in_flight < MAX_IN_FLIGHT && (s.fresh || s.want_key || repeat_due) {
                if s.fresh {
                    s.repeats = 0;
                } else if !s.want_key {
                    s.repeats += 1;
                }
                s.fresh = false;
                let keyframe = std::mem::take(&mut s.want_key);
                s.last_encode = now;
                return Some(Job { frame: s.latest.clone().unwrap(), keyframe });
            }
            s = if s.repeats < MAX_REPEATS && s.in_flight < MAX_IN_FLIGHT && now < repeat_at {
                self.cv.wait_timeout(s, repeat_at - now).unwrap().0
            } else {
                self.cv.wait(s).unwrap()
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::Arc, thread};

    #[test]
    fn newest_frame_wins() {
        let g = Gate::new();
        g.push(1);
        g.push(2);
        let j = g.next().unwrap();
        assert_eq!((j.frame, j.keyframe), (2, false));
    }

    #[test]
    fn waits_for_acks_and_repeats_when_idle() {
        let g = Arc::new(Gate::new());
        g.push(1);
        g.next().unwrap();
        g.sent();
        g.push(2);
        g.next().unwrap();
        g.sent();
        g.push(3); // two frames unacknowledged: 3 must wait

        let g2 = g.clone();
        let t = Instant::now();
        let h = thread::spawn(move || g2.next().unwrap().frame);
        thread::sleep(Duration::from_millis(30));
        g.ack();
        assert_eq!(h.join().unwrap(), 3);
        assert!(t.elapsed() >= Duration::from_millis(30));
        g.sent();
        g.ack();
        g.ack();

        // nothing new: the same frame comes back after REPEAT_AFTER, MAX_REPEATS times
        let t = Instant::now();
        for _ in 0..MAX_REPEATS {
            assert_eq!(g.next().unwrap().frame, 3);
        }
        assert!(t.elapsed() >= REPEAT_AFTER * MAX_REPEATS);
        g.close();
        assert!(g.next().is_none());
    }

    #[test]
    fn keyframe_request_reencodes_latest() {
        let g = Gate::new();
        g.push(7);
        g.next().unwrap();
        g.request_keyframe();
        let j = g.next().unwrap();
        assert_eq!((j.frame, j.keyframe), (7, true));
    }
}
