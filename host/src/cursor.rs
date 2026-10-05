//! macOS: sends the Mac's mouse cursor to the tablet on its own, ahead of the video.
//!
//! The tablet draws it on top of the picture, so moving the mouse over the tablet's display
//! only waits for this poll and the USB hop, not for capture, encode, decode and a video
//! frame's trip to the screen (the same trick Sunshine/Moonlight and remote desktops use).

use objc::runtime::Object;
use objc::{class, msg_send, sel, sel_impl};
use std::{
    ffi::c_void,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use crate::protocol;

/// How often the position is read: the poll is a few µs, and every ms of it is cursor latency.
const POLL: Duration = Duration::from_millis(1);
/// How often the cursor's shape is checked (arrow, I-beam, resize, ...).
const SHAPE_EVERY: Duration = Duration::from_millis(50);

#[repr(C)]
#[derive(Clone, Copy, PartialEq)]
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
    fn CGEventCreate(source: *const c_void) -> *mut c_void;
    fn CGEventGetLocation(event: *const c_void) -> CGPoint;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(cf: *const c_void);
}

#[link(name = "AppKit", kind = "framework")]
unsafe extern "C" {}

/// Stops the poller when dropped.
pub struct CursorSender {
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl CursorSender {
    pub fn start(display_id: u32, tx: mpsc::Sender<Vec<u8>>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let stop = stop.clone();
            thread::spawn(move || {
                unsafe {
                    libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE, 0);
                }
                let mut last_pos: Option<(u16, u16, bool)> = None;
                let mut last_shape: Option<Vec<u8>> = None;
                let mut shape_at = Instant::now() - SHAPE_EVERY;
                while !stop.load(Ordering::Relaxed) {
                    let b = unsafe { CGDisplayBounds(display_id) };
                    if shape_at.elapsed() >= SHAPE_EVERY {
                        shape_at = Instant::now();
                        if let Some(shape) = current_shape(b.size.x)
                            && last_shape.as_ref() != Some(&shape)
                        {
                            if tx.send(shape.clone()).is_err() {
                                return;
                            }
                            last_shape = Some(shape);
                        }
                    }
                    if let Some(p) = location() {
                        let x = (p.x - b.origin.x) / b.size.x;
                        let y = (p.y - b.origin.y) / b.size.y;
                        let on = (0.0..1.0).contains(&x) && (0.0..1.0).contains(&y) && b.size.x > 0.0;
                        let q = |v: f64| (v.clamp(0.0, 1.0) * 65535.0).round() as u16;
                        let pos = (q(x), q(y), on);
                        // Off the tablet's display only the "hidden" change matters.
                        let changed = match last_pos {
                            Some(l) if !on && !l.2 => false,
                            Some(l) => l != pos,
                            None => true,
                        };
                        if changed {
                            if tx.send(protocol::cursor_msg(x, y, on)).is_err() {
                                return;
                            }
                            last_pos = Some(pos);
                        }
                    }
                    thread::sleep(POLL);
                }
            })
        };
        Self { stop, thread: Some(thread) }
    }
}

impl Drop for CursorSender {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// The mouse position in global display points (top-left origin).
fn location() -> Option<CGPoint> {
    unsafe {
        let e = CGEventCreate(std::ptr::null());
        if e.is_null() {
            return None;
        }
        let p = CGEventGetLocation(e);
        CFRelease(e);
        Some(p)
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct NSSize {
    width: f64,
    height: f64,
}

/// The cursor currently shown by any app, as a MSG_CURSOR_IMAGE message.
fn current_shape(display_width_pt: f64) -> Option<Vec<u8>> {
    type Id = *mut Object;
    unsafe {
        let pool: Id = msg_send![class!(NSAutoreleasePool), new];
        let result = (|| {
            let cursor: Id = msg_send![class!(NSCursor), currentSystemCursor];
            if cursor.is_null() {
                return None;
            }
            let image: Id = msg_send![cursor, image];
            if image.is_null() {
                return None;
            }
            let hot: CGPoint = msg_send![cursor, hotSpot];
            let size: NSSize = msg_send![image, size];
            let nil: Id = std::ptr::null_mut();
            let cg: *mut c_void =
                msg_send![image, CGImageForProposedRect: std::ptr::null_mut::<CGRect>() context: nil hints: nil];
            if cg.is_null() {
                return None;
            }
            let rep: Id = msg_send![class!(NSBitmapImageRep), alloc];
            let rep: Id = msg_send![rep, initWithCGImage: cg];
            if rep.is_null() {
                return None;
            }
            let props: Id = msg_send![class!(NSDictionary), dictionary];
            // NSBitmapImageFileTypePNG = 4
            let data: Id = msg_send![rep, representationUsingType: 4usize properties: props];
            let png = if data.is_null() {
                None
            } else {
                let bytes: *const u8 = msg_send![data, bytes];
                let len: usize = msg_send![data, length];
                Some(std::slice::from_raw_parts(bytes, len).to_vec())
            };
            let _: () = msg_send![rep, release];
            let pt = |v: f64| v.round().clamp(0.0, 65535.0) as u16;
            png.map(|png| {
                protocol::cursor_image_msg(
                    pt(display_width_pt),
                    (pt(size.width), pt(size.height)),
                    (pt(hot.x), pt(hot.y)),
                    &png,
                )
            })
        })();
        let _: () = msg_send![pool, drain];
        result
    }
}
