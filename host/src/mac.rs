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

use crate::{
    Args, Button, Host, Input, Pointer, Stream, StreamConfig, capture, cursor,
    frames::{Buffer, Frame, Frames, Region},
    gate::Gate,
    keys, protocol,
    tiles::Picture,
    vt,
};
use screencapturekit::cv::CVPixelBufferLockGuard;
use std::ffi::c_void;

/// Owns the virtual display and keeps it alive for a while after the tablet goes away,
/// so a quick reconnect reuses it and macOS does not shuffle windows back to the main screen.
pub struct MacHost {
    vd: vdisplay_ffi::VDisplay,
    current: Option<((u32, u32, u32), String, u32)>, // (w, h, fps), name, display id
    idle_since: Option<Instant>,
}

impl MacHost {
    pub fn new() -> Self {
        Self { vd: vdisplay_ffi::VDisplay::new(), current: None, idle_since: None }
    }

    fn display(&mut self, args: &Args, w: u32, h: u32, fps: u32, name: &str) -> Result<u32> {
        self.idle_since = None;
        if let Some((mode, current_name, id)) = &self.current {
            let id = *id;
            if *mode == (w, h, fps) && current_name == name {
                println!("reusing the virtual display");
                return Ok(id);
            }
            self.shutdown();
        }
        let d = self.vd.create_virtual_display(w, h, fps as f64, !args.no_hidpi, name, args.ppi, false);
        if d.display_id == 0 {
            bail!("failed to create the virtual display");
        }
        self.current = Some(((w, h, fps), name.to_owned(), d.display_id));
        Ok(d.display_id)
    }
}

impl Buffer for CVPixelBuffer {
    type Pic<'a> = MacPic<'a>;
    fn picture(&self) -> Option<MacPic<'_>> {
        let lock = self.lock_read_only().ok()?;
        (lock.plane_count() == 2).then_some(MacPic(lock))
    }
}

pub struct MacPic<'a>(CVPixelBufferLockGuard<'a>);

impl Picture for MacPic<'_> {
    fn size(&self) -> (usize, usize) {
        (self.0.width_of_plane(0), self.0.height_of_plane(0))
    }
    fn row(&self, plane: usize, row: usize) -> Option<&[u8]> {
        self.0.plane_row(plane, row)
    }
}

/// Stops capture first (no more frames), then the encoder thread.
struct Running {
    /// Measurement aid (`TD_TEST_WINDOW`): an animated window on the virtual display.
    test_window: Option<std::process::Child>,
    capture: Option<capture::Capture>,
    cursor: Option<cursor::CursorSender>,
    /// The brightness keys set the tablet's brightness while the mouse is on it.
    keys: Option<keys::BrightnessKeys>,
    gate: Arc<Gate<Frame<CVPixelBuffer>>>,
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
        drop(self.keys.take());
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
        // Named after the tablet: a monitor of its own to macOS (see vdisplay-ffi's serial).
        let display_id = self.display(args, w, h, fps, &format!("Tablet {}", cfg.tablet))?;
        let frames = Frames::<CVPixelBuffer>::new(timing.clone(), w, h);
        let gate = frames.gate.clone();

        // The frame being encoded (pts, changed area, part of the screen): encoding is
        // synchronous, so the output callback reads it back.
        let full = Region { x: 0, y: 0, w, h };
        let sending = Arc::new(std::sync::Mutex::new((0u64, protocol::ALL, full)));
        // One encoder per size: the whole screen, and parts of it (a video's window).
        let new_encoder = {
            let (tx, frames, sending) = (tx.clone(), frames.clone(), sending.clone());
            move |w: u32, h: u32| {
                let (tx, frames, sending) = (tx.clone(), frames.clone(), sending.clone());
                vt::VtEncoder::new(w, h, fps, bitrate, move |au, _| {
                    let (pts, area, region) = *sending.lock().unwrap();
                    frames.encoded(&tx, pts, area, region, &au);
                })
            }
        };
        let mut encoders = std::collections::HashMap::new();
        encoders.insert((w, h), new_encoder(w, h)?);
        // encoder: takes the newest captured frame whenever the tablet can take another one
        let encode_thread = {
            let frames = frames.clone();
            let tx = tx.clone();
            // Parts of the screen only for a tablet that places pictures itself (the one that
            // takes tiles); TD_REGIONS=none: always the whole screen (debugging).
            let regions = use_tiles && std::env::var("TD_REGIONS").map_or(true, |v| v != "none");
            thread::spawn(move || {
                let gate = frames.gate.clone();
                frames.run(use_tiles, regions, tx, |f, pts, area, region, keyframe| {
                    *sending.lock().unwrap() = (pts, area, region);
                    let whole = |encoders: &std::collections::HashMap<(u32, u32), vt::VtEncoder>, keyframe| {
                        *sending.lock().unwrap() = (pts, area, full);
                        encoders[&(w, h)].encode(f.buf.as_ptr(), pts, keyframe);
                    };
                    if region == full {
                        whole(&encoders, keyframe);
                        return;
                    }
                    // A few sizes at most (a session holds encoder memory): the oldest go.
                    if !encoders.contains_key(&(region.w, region.h)) {
                        if encoders.len() >= 4 {
                            encoders.retain(|k, _| *k == (w, h));
                        }
                        match new_encoder(region.w, region.h) {
                            Ok(e) => {
                                encoders.insert((region.w, region.h), e);
                            }
                            Err(e) => eprintln!("encoder for {}x{}: {e:#}", region.w, region.h),
                        }
                    }
                    let ok = encoders.get(&(region.w, region.h)).is_some_and(|e| {
                        e.encode_crop(f.buf.as_ptr(), region.x as usize, region.y as usize, pts, keyframe)
                    });
                    if !ok {
                        // The whole screen instead, from scratch; and the next frame too, so the
                        // part's encoder starts over as well.
                        eprintln!("could not encode the {}x{} part at {},{}: the whole screen instead", region.w, region.h, region.x, region.y);
                        whole(&encoders, true);
                        gate.request_keyframe();
                    }
                })
            })
        };
        let mut running = Running { test_window: None, capture: None, cursor: None, keys: None, gate: gate.clone(), encode_thread: Some(encode_thread) };

        // The cursor goes to the tablet separately, ahead of the video, unless asked otherwise.
        if !args.cursor_in_video {
            running.cursor = Some(cursor::CursorSender::start(display_id, tx.clone()));
        }
        running.keys = keys::BrightnessKeys::start(display_id, cfg.brightness.clone());
        running.capture = Some(capture::Capture::start(
            display_id,
            w,
            h,
            fps,
            args.cursor_in_video,
            audio_on.load(Ordering::Relaxed),
            {
                let frames = frames.clone();
                move |buf, age, dirty| frames.push(buf, age.map(|a| timing.ago(a)), dirty)
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
