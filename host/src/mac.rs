//! macOS backend: CoreGraphics virtual display -> ScreenCaptureKit -> VideoToolbox.

use anyhow::{Result, bail};
use screencapturekit::CVPixelBuffer;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use crate::{Args, Button, Control, Host, Input, Pointer, Stream, StreamConfig, capture, cursor, gate::Gate, protocol, vt};
use std::ffi::c_void;

/// A pixel rectangle as 0..=65535 fractions of the frame, rounded outwards.
fn normalize(r: [f64; 4], w: u32, h: u32) -> [u16; 4] {
    let n = |v: f64, size: u32, up: bool| {
        let f = (v / size as f64 * 65535.0).clamp(0.0, 65535.0);
        (if up { f.ceil() } else { f.floor() }) as u16
    };
    [n(r[0], w, false), n(r[1], h, false), n(r[2], w, true), n(r[3], h, true)]
}

/// Owns the virtual display and keeps it alive for a while after the tablet goes away,
/// so a quick reconnect reuses it and macOS does not shuffle windows back to the main screen.
pub struct MacHost {
    vd: vdisplay_ffi::VDisplay,
    current: Option<((u32, u32, u32), u32)>, // (w, h, fps), display id
    idle_since: Option<Instant>,
}

impl MacHost {
    pub fn new() -> Self {
        Self { vd: vdisplay_ffi::VDisplay::new(), current: None, idle_since: None }
    }

    fn display(&mut self, args: &Args, w: u32, h: u32, fps: u32) -> Result<u32> {
        self.idle_since = None;
        if let Some((mode, id)) = self.current {
            if mode == (w, h, fps) {
                println!("reusing the virtual display");
                return Ok(id);
            }
            self.shutdown();
        }
        let d = self.vd.create_virtual_display(w, h, fps as f64, !args.no_hidpi, "Tablet", args.ppi, false);
        if d.display_id == 0 {
            bail!("failed to create the virtual display");
        }
        self.current = Some(((w, h, fps), d.display_id));
        Ok(d.display_id)
    }
}

/// A captured frame and when it was composited / handed to us (session clock, µs).
#[derive(Clone)]
struct Frame {
    buf: CVPixelBuffer,
    composited: Option<u64>,
    delivered: u64,
}

impl Control for Gate<Frame> {
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

/// Stops capture first (no more frames), then the encoder thread.
struct Running {
    /// Measurement aid (`TD_TEST_WINDOW`): an animated window on the virtual display.
    test_window: Option<std::process::Child>,
    capture: Option<capture::Capture>,
    cursor: Option<cursor::CursorSender>,
    gate: Arc<Gate<Frame>>,
    encode_thread: Option<thread::JoinHandle<()>>,
}

impl Drop for Running {
    fn drop(&mut self) {
        // First, and waited for: the window must be gone before the display can go away,
        // or macOS moves it onto another screen.
        if let Some(mut w) = self.test_window.take() {
            let _ = w.kill();
            let _ = w.wait();
        }
        drop(self.cursor.take());
        drop(self.capture.take());
        self.gate.close();
        if let Some(t) = self.encode_thread.take() {
            let _ = t.join();
        }
    }
}

impl Host for MacHost {
    fn start(
        &mut self,
        args: &Args,
        cfg: &StreamConfig,
        audio_on: Arc<AtomicBool>,
        tx: mpsc::Sender<Vec<u8>>,
    ) -> Result<Stream> {
        let StreamConfig { width: w, height: h, fps, bitrate, .. } = *cfg;
        let timing = cfg.timing.clone();
        // Debugging: TD_TILES=none sends everything through H.264.
        let use_tiles = cfg.tiles && std::env::var("TD_TILES").map_or(true, |v| v != "none");
        let display_id = self.display(args, w, h, fps)?;
        let gate = Arc::new(Gate::<Frame>::new());

        // When the frame being encoded was handed to the encoder; encoding is synchronous,
        // so the output callback reads it back as the frame's timestamp.
        let submitted = Arc::new(std::sync::atomic::AtomicU64::new(0));
        // Area changed since the last encoded frame (pixels: x0, y0, x1, y1), including frames
        // flow control skipped; and the one sent with the frame being encoded.
        let changed = Arc::new(std::sync::Mutex::new(Vec::<crate::tiles::Rect>::new()));
        let sending = Arc::new(std::sync::Mutex::new(protocol::ALL));
        let encoder = {
            let tx = tx.clone();
            let gate = gate.clone();
            let submitted = submitted.clone();
            let sending = sending.clone();
            let timing = timing.clone();
            vt::VtEncoder::new(w, h, fps, bitrate, move |au, _| {
                gate.sent();
                let pts = submitted.load(Ordering::Relaxed);
                timing.encoded(pts, au.len());
                tx.send(protocol::video_msg(pts, *sending.lock().unwrap(), &au)).ok();
            })?
        };
        // encoder: takes the newest captured frame whenever the tablet can take another one
        let encode_thread = {
            let gate = gate.clone();
            let timing = timing.clone();
            let changed = changed.clone();
            let tx = tx.clone();
            thread::spawn(move || {
                let mut first = true;
                let mut shadow = crate::tiles::Shadow::new();
                // Areas last sent through H.264 since the idle repeats last ran: only those need
                // re-sharpening (tiles are exact), and none at all after pure typing.
                let mut lossy: Option<[f64; 4]> = None;
                // Tiles went out since the last H.264 frame: the encoder's reference no longer
                // matches the screen, and a P-frame copying "unchanged" blocks from it would
                // put stale pictures (old text, old bar positions) on the tablet.
                let mut stale_reference = false;
                let mut repeats = 0;
                unsafe {
                    libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE, 0);
                }
                while let Some(job) = gate.next() {
                    let f = &job.frame;
                    let reported = std::mem::take(&mut *changed.lock().unwrap());
                    // What really changed inside what macOS reported (repeats re-send the same frame).
                    let rects = if job.repeat { reported } else { shadow.changed(&f.buf, &reported).unwrap_or(reported) };
                    let area = crate::tiles::bounds(&rects);
                    let full = job.keyframe || first;
                    let rect = if full {
                        None
                    } else if job.repeat {
                        repeats += 1;
                        let r = lossy;
                        if repeats >= crate::gate::MAX_REPEATS {
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
                        if let Some(msgs) = use_tiles.then(|| crate::tiles::build_all(&f.buf, &rects, now)).flatten() {
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
                    first = false;
                    let now = timing.now();
                    submitted.store(now, Ordering::Relaxed);
                    timing.encode_started(now, f.composited, Some(f.delivered), job.repeat);
                    *sending.lock().unwrap() = rect.map_or(protocol::ALL, |r| normalize(r, w, h));
                    let keyframe = job.keyframe || std::mem::take(&mut stale_reference);
                    encoder.encode(f.buf.as_ptr(), now, keyframe);
                }
            })
        };
        let mut running = Running { test_window: None, capture: None, cursor: None, gate: gate.clone(), encode_thread: Some(encode_thread) };

        // The cursor goes to the tablet separately, ahead of the video, unless asked otherwise.
        if !args.cursor_in_video {
            running.cursor = Some(cursor::CursorSender::start(display_id, tx.clone()));
        }
        let capture_gate = gate.clone();
        running.capture = Some(capture::Capture::start(
            display_id,
            w,
            h,
            fps,
            args.cursor_in_video,
            move |buf, age, dirty| {
                let mut c = changed.lock().unwrap();
                for r in dirty.unwrap_or_else(|| vec![[0.0, 0.0, w as f64, h as f64]]) {
                    crate::tiles::add(&mut c, r);
                }
                drop(c);
                let delivered = timing.now();
                timing.captured();
                let composited = age.map(|a| timing.ago(a));
                capture_gate.push(Frame { buf, composited, delivered })
            },
            move |pcm| {
                if audio_on.load(Ordering::Relaxed) {
                    tx.send(protocol::audio_msg(pcm)).ok();
                }
            },
        )?);
        // Measurement aid: TD_TEST_WINDOW=<binary> runs `<binary> <display id>` for this session only.
        if let Ok(bin) = std::env::var("TD_TEST_WINDOW") {
            running.test_window = std::process::Command::new(bin).arg(display_id.to_string()).spawn().ok();
        }
        let input = MouseInput::new(display_id);
        Ok(Stream { control: gate, input: Some(Box::new(input)), guard: Box::new(running) })
    }

    fn release(&mut self) {
        self.idle_since = Some(Instant::now());
    }

    fn expire(&mut self, after: Duration) {
        if self.idle_since.is_some_and(|t| t.elapsed() >= after) {
            self.shutdown();
        }
    }

    fn shutdown(&mut self) {
        if self.current.take().is_some() {
            self.vd.destroy_virtual_display();
            println!("virtual display removed");
        }
        self.idle_since = None;
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CGPoint {
    x: f64,
    y: f64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CGRect {
    origin: CGPoint,
    size: CGPoint,
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGDisplayBounds(display: u32) -> CGRect;
    fn CGEventCreateMouseEvent(source: *const c_void, kind: u32, pos: CGPoint, button: u32) -> *mut c_void;
    fn CGEventCreateScrollWheelEvent2(
        source: *const c_void,
        units: u32,
        wheel_count: u32,
        wheel1: i32,
        wheel2: i32,
        wheel3: i32,
    ) -> *mut c_void;
    fn CGEventSetIntegerValueField(event: *mut c_void, field: u32, value: i64);
    fn CGEventPost(tap: u32, event: *mut c_void);
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> u8;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(cf: *const c_void);
}

const LEFT_DOWN: u32 = 1;
const LEFT_UP: u32 = 2;
const RIGHT_DOWN: u32 = 3;
const RIGHT_UP: u32 = 4;
const MOVED: u32 = 5;
const LEFT_DRAGGED: u32 = 6;
const RIGHT_DRAGGED: u32 = 7;
const FIELD_CLICK_STATE: u32 = 1;
const HID_EVENT_TAP: u32 = 0;
const SCROLL_UNIT_PIXEL: u32 = 0;

/// Posts the tablet's touches as mouse events on the virtual display (needs the
/// Accessibility permission for the terminal running the host).
struct MouseInput {
    display_id: u32,
    held: Option<Button>,
}

impl MouseInput {
    fn new(display_id: u32) -> Self {
        if unsafe { AXIsProcessTrusted() } == 0 {
            eprintln!(
                "touch input needs the Accessibility permission: System Settings > Privacy & Security > \
                 Accessibility, enable your terminal, then restart the host"
            );
        }
        Self { display_id, held: None }
    }

    fn post(&self, event: *mut c_void) {
        if !event.is_null() {
            unsafe {
                CGEventPost(HID_EVENT_TAP, event);
                CFRelease(event);
            }
        }
    }
}

impl Input for MouseInput {
    fn pointer(&mut self, ev: Pointer, x: f64, y: f64, clicks: u32) {
        let b = unsafe { CGDisplayBounds(self.display_id) };
        let pos = CGPoint { x: b.origin.x + x * b.size.x, y: b.origin.y + y * b.size.y };
        let (kind, button) = match ev {
            Pointer::Move => (MOVED, 0),
            Pointer::Down(Button::Left) => (LEFT_DOWN, 0),
            Pointer::Down(Button::Right) => (RIGHT_DOWN, 1),
            Pointer::Up(Button::Left) => (LEFT_UP, 0),
            Pointer::Up(Button::Right) => (RIGHT_UP, 1),
            Pointer::Drag => match self.held {
                Some(Button::Right) => (RIGHT_DRAGGED, 1),
                Some(Button::Left) => (LEFT_DRAGGED, 0),
                None => (MOVED, 0),
            },
        };
        match ev {
            Pointer::Down(b) => self.held = Some(b),
            Pointer::Up(_) => self.held = None,
            _ => {}
        }
        unsafe {
            let e = CGEventCreateMouseEvent(std::ptr::null(), kind, pos, button);
            if !e.is_null() && kind != MOVED {
                CGEventSetIntegerValueField(e, FIELD_CLICK_STATE, clicks as i64);
            }
            self.post(e);
        }
    }

    fn scroll(&mut self, dx: f64, dy: f64) {
        // Positive wheel values scroll content down/right, i.e. follow the fingers.
        let e = unsafe {
            CGEventCreateScrollWheelEvent2(std::ptr::null(), SCROLL_UNIT_PIXEL, 2, dy as i32, dx as i32, 0)
        };
        self.post(e);
    }
}
