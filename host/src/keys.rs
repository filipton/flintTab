//! macOS: the brightness keys set the tablet's brightness while the mouse is on its display
//! (macOS cannot dim a virtual display itself); anywhere else they work as usual.
//!
//! The keys arrive as NX_SYSDEFINED events, caught with an event tap (needs the Accessibility
//! permission, like touch input). ⌥⇧ with a key: quarter steps, as macOS does.

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
    time::Duration,
};

use crate::brightness::Brightness;

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

type TapCallback = extern "C" fn(*mut c_void, u32, *mut c_void, *mut c_void) -> *mut c_void;

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGEventTapCreate(tap: u32, place: u32, options: u32, mask: u64, callback: TapCallback, info: *mut c_void) -> *mut c_void;
    fn CGEventTapEnable(tap: *mut c_void, enable: bool);
    fn CGEventGetFlags(event: *mut c_void) -> u64;
    fn CGDisplayBounds(display: u32) -> CGRect;
    fn CGEventCreate(source: *const c_void) -> *mut c_void;
    fn CGEventGetLocation(event: *const c_void) -> CGPoint;
    fn CGGetOnlineDisplayList(max: u32, displays: *mut u32, count: *mut u32) -> i32;
    fn CGDisplayIsBuiltin(display: u32) -> u32;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFRunLoopCommonModes: *const c_void;
    fn CFMachPortCreateRunLoopSource(alloc: *const c_void, port: *mut c_void, order: isize) -> *mut c_void;
    fn CFMachPortInvalidate(port: *mut c_void);
    fn CFRunLoopGetCurrent() -> *mut c_void;
    fn CFRunLoopAddSource(rl: *mut c_void, source: *mut c_void, mode: *const c_void);
    fn CFRunLoopRun();
    fn CFRunLoopStop(rl: *mut c_void);
    fn CFRelease(cf: *const c_void);
}

const SESSION_EVENT_TAP: u32 = 1;
const HEAD_INSERT: u32 = 0;
const NX_SYSDEFINED: u32 = 14;
const TAP_DISABLED_BY_TIMEOUT: u32 = 0xFFFF_FFFE;
const TAP_DISABLED_BY_USER_INPUT: u32 = 0xFFFF_FFFF;
/// NSEvent subtype of the special keys (brightness, volume, media).
const AUX_CONTROL_BUTTONS: i16 = 8;
const KEY_BRIGHTNESS_UP: isize = 2;
const KEY_BRIGHTNESS_DOWN: isize = 3;
const KEY_DOWN: isize = 0xA;
const FLAG_SHIFT: u64 = 0x20000;
const FLAG_OPTION: u64 = 0x80000;

struct Tap {
    display_id: u32,
    brightness: Arc<Brightness>,
    port: *mut c_void,
}

/// The brightness keys drive the tablet while this lives.
pub struct BrightnessKeys {
    run_loop: usize,
    thread: Option<thread::JoinHandle<()>>,
}

impl BrightnessKeys {
    /// None when the tap cannot be made (no Accessibility permission).
    pub fn start(display_id: u32, brightness: Arc<Brightness>) -> Option<Self> {
        let (ready, started) = mpsc::channel();
        let thread = thread::spawn(move || unsafe {
            let tap = Box::into_raw(Box::new(Tap { display_id, brightness, port: std::ptr::null_mut() }));
            let port = CGEventTapCreate(SESSION_EVENT_TAP, HEAD_INSERT, 0, 1 << NX_SYSDEFINED, on_event, tap.cast());
            if port.is_null() {
                drop(Box::from_raw(tap));
                ready.send(None).ok();
                return;
            }
            (*tap).port = port;
            let source = CFMachPortCreateRunLoopSource(std::ptr::null(), port, 0);
            let rl = CFRunLoopGetCurrent();
            CFRunLoopAddSource(rl, source, kCFRunLoopCommonModes);
            CGEventTapEnable(port, true);
            ready.send(Some(rl as usize)).ok();
            CFRunLoopRun();
            CGEventTapEnable(port, false);
            CFMachPortInvalidate(port);
            CFRelease(source);
            CFRelease(port);
            drop(Box::from_raw(tap));
        });
        match started.recv().ok().flatten() {
            Some(run_loop) => Some(Self { run_loop, thread: Some(thread) }),
            None => {
                thread.join().ok();
                println!("the brightness keys cannot set the tablet's brightness: allow this terminal under System Settings > Privacy & Security > Accessibility");
                None
            }
        }
    }
}

impl Drop for BrightnessKeys {
    fn drop(&mut self) {
        unsafe { CFRunLoopStop(self.run_loop as *mut c_void) };
        if let Some(t) = self.thread.take() {
            t.join().ok();
        }
    }
}

/// Whether the mouse is on the display.
fn mouse_on(display_id: u32) -> bool {
    unsafe {
        let e = CGEventCreate(std::ptr::null());
        if e.is_null() {
            return false;
        }
        let p = CGEventGetLocation(e);
        CFRelease(e);
        let b = CGDisplayBounds(display_id);
        p.x >= b.origin.x && p.x < b.origin.x + b.size.x && p.y >= b.origin.y && p.y < b.origin.y + b.size.y
    }
}

extern "C" fn on_event(_proxy: *mut c_void, kind: u32, event: *mut c_void, info: *mut c_void) -> *mut c_void {
    let tap = unsafe { &*(info as *const Tap) };
    if kind == TAP_DISABLED_BY_TIMEOUT || kind == TAP_DISABLED_BY_USER_INPUT {
        unsafe { CGEventTapEnable(tap.port, true) };
        return event;
    }
    if kind != NX_SYSDEFINED {
        return event;
    }
    let (subtype, data1): (i16, isize) = objc::rc::autoreleasepool(|| unsafe {
        let ns: *mut Object = msg_send![class!(NSEvent), eventWithCGEvent: event];
        if ns.is_null() {
            return (0, 0);
        }
        (msg_send![ns, subtype], msg_send![ns, data1])
    });
    let key = (data1 & 0xFFFF_0000) >> 16;
    if subtype != AUX_CONTROL_BUTTONS || (key != KEY_BRIGHTNESS_UP && key != KEY_BRIGHTNESS_DOWN) || !mouse_on(tap.display_id) {
        return event;
    }
    // Presses and repeats step; releases are swallowed too, so macOS shows nothing.
    if (data1 & 0xFF00) >> 8 == KEY_DOWN {
        let fine = unsafe { CGEventGetFlags(event) } & (FLAG_SHIFT | FLAG_OPTION) == FLAG_SHIFT | FLAG_OPTION;
        let step = if key == KEY_BRIGHTNESS_UP { 1 } else { -1 };
        let level = tap.brightness.step(step, if fine { 64 } else { 16 });
        println!("tablet brightness {level}%");
    }
    std::ptr::null_mut()
}

/// The tablet's brightness follows the Mac's built-in screen: its slider in System Settings and
/// Control Center, its keys and auto-brightness (macOS gives a virtual display no slider).
pub struct FollowMac {
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

/// How often the Mac's level is read (a call to corebrightnessd; a slider drag stays smooth).
const FOLLOW_POLL: Duration = Duration::from_millis(100);

impl FollowMac {
    /// None without a built-in screen (DisplayServices drives no other).
    pub fn start(brightness: Arc<Brightness>) -> Option<Self> {
        let get = display_services_get()?;
        let builtin = builtin_display()?;
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let stop = stop.clone();
            thread::spawn(move || {
                let mut last: Option<u8> = None;
                while !stop.load(Ordering::Relaxed) {
                    let mut v = 0f32;
                    if unsafe { get(builtin, &mut v) } == 0 {
                        let level = (v.clamp(0.0, 1.0) * 100.0).round() as u8;
                        if last != Some(level) {
                            brightness.set(level);
                            last = Some(level);
                        }
                    }
                    thread::sleep(FOLLOW_POLL);
                }
            })
        };
        println!("the tablet's brightness follows this Mac's screen");
        Some(Self { stop, thread: Some(thread) })
    }
}

impl Drop for FollowMac {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            t.join().ok();
        }
    }
}

type GetBrightness = unsafe extern "C" fn(u32, *mut f32) -> i32;

/// DisplayServicesGetBrightness (private; what the slider shows, 0..=1).
fn display_services_get() -> Option<GetBrightness> {
    unsafe {
        let lib = libc::dlopen(c"/System/Library/PrivateFrameworks/DisplayServices.framework/DisplayServices".as_ptr(), libc::RTLD_NOW);
        let f = (!lib.is_null()).then(|| libc::dlsym(lib, c"DisplayServicesGetBrightness".as_ptr())).filter(|f| !f.is_null());
        if f.is_none() {
            println!("cannot follow the Mac's brightness: DisplayServices is missing");
        }
        f.map(|f| std::mem::transmute::<*mut c_void, GetBrightness>(f))
    }
}

fn builtin_display() -> Option<u32> {
    let mut ids = [0u32; 16];
    let mut n = 0u32;
    unsafe { CGGetOnlineDisplayList(16, ids.as_mut_ptr(), &mut n) };
    let found = ids[..n as usize].iter().copied().find(|&d| unsafe { CGDisplayIsBuiltin(d) } != 0);
    if found.is_none() {
        println!("cannot follow the Mac's brightness: this Mac has no built-in screen");
    }
    found
}
