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

use crate::{Args, Button, Control, Host, Input, Pointer, Stream, StreamConfig, capture, gate::Gate, protocol, vt};
use std::ffi::c_void;

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

impl Control for Gate<CVPixelBuffer> {
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
    capture: Option<capture::Capture>,
    gate: Arc<Gate<CVPixelBuffer>>,
    encode_thread: Option<thread::JoinHandle<()>>,
}

impl Drop for Running {
    fn drop(&mut self) {
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
        let StreamConfig { width: w, height: h, fps, bitrate } = *cfg;
        let display_id = self.display(args, w, h, fps)?;
        let gate = Arc::new(Gate::<CVPixelBuffer>::new());

        let started = Instant::now();
        let encoder = {
            let tx = tx.clone();
            let gate = gate.clone();
            vt::VtEncoder::new(w, h, fps, bitrate, move |au, _| {
                gate.sent();
                tx.send(protocol::video_msg(started.elapsed().as_micros() as u64, &au)).ok();
            })?
        };
        // encoder: takes the newest captured frame whenever the tablet can take another one
        let encode_thread = {
            let gate = gate.clone();
            thread::spawn(move || {
                unsafe {
                    libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE, 0);
                }
                while let Some(job) = gate.next() {
                    encoder.encode(job.frame.as_ptr(), started.elapsed().as_micros() as u64, job.keyframe);
                }
            })
        };
        let mut running = Running { capture: None, gate: gate.clone(), encode_thread: Some(encode_thread) };

        let capture_gate = gate.clone();
        running.capture = Some(capture::Capture::start(
            display_id,
            w,
            h,
            fps,
            move |pixel_buffer| capture_gate.push(pixel_buffer),
            move |pcm| {
                if audio_on.load(Ordering::Relaxed) {
                    tx.send(protocol::audio_msg(pcm)).ok();
                }
            },
        )?);
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
