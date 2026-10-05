//! Turns an Android tablet (connected over USB) into a secondary display for macOS or Linux.
//!
//! Flow: `adb reverse` exposes this host's TCP port inside the tablet, the tablet app
//! connects and reports its screen size, we create a virtual display of that size,
//! capture it, encode it to H.264 and stream it back over the same socket.

mod aoa;
mod app;
#[cfg(target_os = "macos")]
mod capture;
#[cfg(target_os = "macos")]
mod cursor;
mod gate;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod mac;
mod protocol;
mod sps;
mod tablet;
#[cfg(target_os = "macos")]
mod tiles;
mod timing;
#[cfg(target_os = "macos")]
mod vt;

use anyhow::Result;
#[allow(unused_imports)]
use anyhow::bail;
use clap::Parser;
use std::{
    io::{Read, Write},
    net::{Shutdown, TcpListener, TcpStream},
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

const APP_ACTIVITY: &str = "dev.tabdisplay/.MainActivity";

#[derive(Parser)]
#[command(about = "Use an Android tablet as a USB secondary display for macOS")]
pub struct Args {
    /// TCP port used between host and tablet (via `adb reverse`)
    #[arg(long, default_value_t = 27183)]
    port: u16,
    /// Frame rate (30-120). Default: the tablet's refresh rate, if it can decode that fast
    #[arg(long)]
    fps: Option<u32>,
    /// Video bitrate in Mbit/s. Default: 25, or 40 above 60 fps
    #[arg(long)]
    bitrate: Option<u32>,
    /// Seconds the virtual display is kept after the tablet disconnects, so windows stay
    /// put if it comes back (app switched, cable re-plugged)
    #[arg(long, default_value_t = 15)]
    keep_display: u64,
    /// Cap on the virtual display width in pixels (height follows the tablet's aspect)
    #[arg(long, default_value_t = 2560)]
    max_width: u32,
    /// Force the virtual display size instead of using the tablet's screen size
    #[arg(long, requires = "height")]
    width: Option<u32>,
    #[arg(long, requires = "width")]
    height: Option<u32>,
    /// Pixel density reported to macOS
    #[arg(long, default_value_t = 220)]
    ppi: i32,
    /// Disable HiDPI (macOS UI becomes very small on a dense tablet screen)
    #[arg(long)]
    no_hidpi: bool,
    /// Path of the adb binary
    #[arg(long, default_value = "adb")]
    adb: String,
    /// adb serial of the tablet (see `adb devices`). Default: the first real device (an emulator only by serial)
    #[arg(long, short = 's')]
    serial: Option<String>,
    /// Do not start the tablet app automatically
    #[arg(long)]
    no_launch: bool,
    /// Tablet app to install (default: tabdisplay.apk next to the host or in the current
    /// folder, a local Gradle build, or else the published build, downloaded)
    #[arg(long)]
    apk: Option<std::path::PathBuf>,
    /// Do not install or update the tablet app
    #[arg(long)]
    no_install: bool,
    /// Leave the tablet's battery saver and motion smoothness alone (by default they are
    /// lifted while the host runs, because both cap the panel at 60 Hz, and restored on exit)
    #[arg(long)]
    keep_tablet_settings: bool,
    /// Stay on adb's TCP forward instead of switching the tablet to a raw USB accessory
    #[arg(long)]
    no_aoa: bool,
    /// Do not run adb at all (the tablet connects some other way, e.g. a test client)
    #[arg(long, hide = true)]
    no_adb: bool,
    /// Linux: stream a GStreamer test pattern instead of a screen (for testing without a desktop)
    #[arg(long, hide = true)]
    test_source: bool,
    /// macOS: draw the mouse cursor into the video instead of sending it separately
    /// (the separate cursor is drawn by the tablet and moves with much less delay)
    #[arg(long)]
    cursor_in_video: bool,
    /// Linux: H.264 encoder element to use instead of the first available one
    /// (nvh264enc, vah264lpenc, vah264enc, vaapih264enc, qsvh264enc, x264enc)
    #[arg(long)]
    encoder: Option<String>,
    /// Linux: capture this X11 screen area (X,Y; size = the tablet's) instead of a portal virtual monitor
    #[arg(long)]
    x11_region: Option<String>,
    /// Linux: let the portal pick an existing monitor instead of creating a virtual one
    #[arg(long)]
    portal_monitor: bool,
}

fn even(v: u32) -> u32 {
    (v.max(2)) & !1
}

fn pick_size(args: &Args, dev_w: u32, dev_h: u32) -> (u32, u32) {
    if let (Some(w), Some(h)) = (args.width, args.height) {
        return (even(w), even(h));
    }
    let (w, h) = if dev_w > args.max_width {
        (args.max_width, (dev_h as u64 * args.max_width as u64 / dev_w as u64) as u32)
    } else {
        (dev_w, dev_h)
    };
    (even(w), even(h))
}

/// The device to talk to: `wanted` if it is attached, else the first real device. Emulators
/// only when asked for by serial (a tablet re-enumerating would otherwise hand over to one).
fn pick_device(adb: &str, wanted: Option<&str>) -> Option<String> {
    let out = Command::new(adb).arg("devices").stderr(Stdio::null()).output().ok()?;
    let ready: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .skip(1)
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let serial = it.next()?;
            (it.next() == Some("device")).then(|| serial.to_owned())
        })
        .collect();
    match wanted {
        Some(w) => ready.into_iter().find(|s| s == w),
        None => ready.into_iter().find(|s| !s.starts_with("emulator-")),
    }
}

/// Keeps `adb reverse` alive (it is dropped on unplug), and on connect installs or
/// updates the app and opens it.
fn spawn_adb_watcher(args: &Args, busy: Arc<AtomicBool>, current: Arc<std::sync::Mutex<Option<String>>>) {
    let adb = args.adb.clone();
    let wanted = args.serial.clone();
    let port = args.port;
    let launch = !args.no_launch;
    let install = !args.no_install;
    let tweak = !args.keep_tablet_settings;
    let apk = args.apk.clone();
    thread::spawn(move || {
        let mut connected: Option<String> = None;
        let mut tweaked: Option<String> = None;
        loop {
            if !busy.load(Ordering::Relaxed) {
                let spec = format!("tcp:{port}");
                let device = pick_device(&adb, wanted.as_deref());
                // Before the port forward, so the app cannot connect while the caps still apply.
                if tweak && device != tweaked {
                    if let Some(serial) = device.as_deref() {
                        tablet::apply(&adb, serial);
                    }
                    tweaked = device.clone();
                }
                let device = device.filter(|serial| {
                    Command::new(&adb)
                        .args(["-s", serial, "reverse", &spec, &spec])
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .status()
                        .map(|s| s.success())
                        .unwrap_or(false)
                });
                if let Some(serial) = device.as_deref()
                    && connected.as_deref() != Some(serial)
                {
                    println!("tablet detected over USB ({serial})");
                    if install {
                        match app::find_apk(apk.as_deref()) {
                            Some(path) => app::ensure_installed(&adb, serial, &path),
                            None => eprintln!("no tablet app to install; pass --apk or build android/"),
                        }
                    }
                    if launch {
                        let _ = Command::new(&adb)
                            .args(["-s", serial, "shell", "am", "start", "-n", APP_ACTIVITY])
                            .stdout(Stdio::null())
                            .stderr(Stdio::null())
                            .status();
                    }
                }
                *current.lock().unwrap() = device.clone();
                connected = device;
            }
            thread::sleep(Duration::from_secs(2));
        }
    });
}

/// What a session needs from the platform's video pipeline while it runs.
pub trait Control: Send + Sync {
    /// The tablet took one frame off the wire.
    fn ack(&self);
    /// The tablet's decoder restarted: the next frame must be a keyframe.
    fn request_keyframe(&self);
    /// The connection is gone; stop producing frames.
    fn close(&self);
}

pub struct StreamConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate: u32,
    /// Session clock: video pts are microseconds since this instant.
    pub epoch: std::time::Instant,
    /// Per-stage latency on the session clock.
    pub timing: Arc<timing::Timing>,
    /// The tablet draws small updates sent as pixels (MSG_TILE).
    pub tiles: bool,
}

/// A running capture + encode pipeline; dropping `guard` stops it.
pub struct Stream {
    pub control: Arc<dyn Control>,
    /// Injects the tablet's touches and pen as mouse input, where the platform allows it.
    pub input: Option<Box<dyn Input>>,
    pub guard: Box<dyn std::any::Any>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Button {
    Left,
    Right,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Pointer {
    Move,
    Down(Button),
    Drag,
    Up(Button),
}

/// Mouse input on the virtual display. `x`/`y` are 0..=1 across the display.
pub trait Input: Send {
    /// `clicks` is 2 for the second press of a double click, etc. (macOS needs it).
    fn pointer(&mut self, ev: Pointer, x: f64, y: f64, clicks: u32);
    /// Finger movement in display pixels; positive dy = fingers moved down (content follows).
    fn scroll(&mut self, dx: f64, dy: f64);
}

/// Turns control messages into `Input` calls and counts multi-clicks.
struct InputDecoder {
    input: Box<dyn Input>,
    last_down: Option<(std::time::Instant, f64, f64, u32)>,
}

impl InputDecoder {
    fn handle(&mut self, kind: u8, value: u8, payload: &[u8]) {
        let a = u16::from_be_bytes([payload[0], payload[1]]);
        let b = u16::from_be_bytes([payload[2], payload[3]]);
        if kind == protocol::KIND_SCROLL {
            self.input.scroll(a as i16 as f64, b as i16 as f64);
            return;
        }
        let (x, y) = (a as f64 / 65535.0, b as f64 / 65535.0);
        let ev = match value {
            protocol::POINTER_MOVE => Pointer::Move,
            protocol::POINTER_LEFT_DOWN => Pointer::Down(Button::Left),
            protocol::POINTER_DRAG => Pointer::Drag,
            protocol::POINTER_LEFT_UP => Pointer::Up(Button::Left),
            protocol::POINTER_RIGHT_DOWN => Pointer::Down(Button::Right),
            protocol::POINTER_RIGHT_UP => Pointer::Up(Button::Right),
            _ => return,
        };
        let mut clicks = self.last_down.map_or(1, |l| l.3);
        if ev == Pointer::Down(Button::Left) {
            let now = std::time::Instant::now();
            clicks = match self.last_down {
                Some((t, lx, ly, n))
                    if now.duration_since(t) < Duration::from_millis(400)
                        && (x - lx).abs() < 0.01
                        && (y - ly).abs() < 0.01 =>
                {
                    n + 1
                }
                _ => 1,
            };
            self.last_down = Some((now, x, y, clicks));
        }
        self.input.pointer(ev, x, y, clicks);
    }
}

/// A platform backend. It owns the virtual display, which outlives single sessions.
pub trait Host {
    /// Makes sure a virtual display of this size exists and starts streaming it: every
    /// encoded access unit goes to `tx` as a `protocol::video_msg`, audio as `audio_msg`
    /// while `audio_on` is set.
    fn start(
        &mut self,
        args: &Args,
        cfg: &StreamConfig,
        audio_on: Arc<AtomicBool>,
        tx: mpsc::Sender<Vec<u8>>,
    ) -> Result<Stream>;
    /// The tablet disconnected; keep the display for now.
    fn release(&mut self);
    /// Removes the display once it has been unused for `after`.
    fn expire(&mut self, after: Duration);
    fn shutdown(&mut self);
}

/// A connection to the tablet app: adb's TCP forward, or the USB accessory.
struct Conn {
    reader: Box<dyn Read + Send>,
    writer: Box<dyn Write + Send>,
    /// Unblocks the reader and writer for good.
    close: Box<dyn Fn() + Send>,
    /// Called once the handshake is in (lifts its timeout).
    ready: Box<dyn FnOnce() + Send>,
    via: &'static str,
}

impl Conn {
    fn tcp(sock: TcpStream) -> Result<Self> {
        sock.set_nonblocking(false)?;
        sock.set_nodelay(true)?;
        sock.set_read_timeout(Some(Duration::from_secs(5)))?; // for the handshake
        let closer = sock.try_clone()?;
        let untimed = sock.try_clone()?;
        Ok(Self {
            ready: Box::new(move || {
                let _ = untimed.set_read_timeout(None);
            }),
            reader: Box::new(sock.try_clone()?),
            writer: Box::new(sock),
            close: Box::new(move || {
                let _ = closer.shutdown(Shutdown::Both);
            }),
            via: "adb",
        })
    }
}

/// Opens the USB accessory whenever the tablet is attached and no session runs, and hands it
/// over once the tablet app talks on it (until then a TCP session can start instead).
fn spawn_aoa(running: Arc<AtomicBool>, busy: Arc<AtomicBool>, current: Arc<std::sync::Mutex<Option<String>>>) -> mpsc::Receiver<Conn> {
    let (tx, rx) = mpsc::sync_channel(0);
    thread::spawn(move || {
        let mut last: Option<aoa::Link> = None;
        while running.load(Ordering::Relaxed) {
            let serial = current.lock().unwrap().clone();
            // The app reconnected during the last session (e.g. its panel rate changed): its
            // handshake already came in on that link.
            let link = match (last.take(), serial) {
                (Some(l), _) if l.reconnected() => Some(l),
                (_, Some(s)) if !busy.load(Ordering::Relaxed) => aoa::open(&s),
                _ => None,
            };
            let Some(link) = link else {
                thread::sleep(Duration::from_millis(500));
                continue;
            };
            let mut reader = link.reader();
            let stop = || !running.load(Ordering::Relaxed) || busy.load(Ordering::Relaxed);
            if reader.wait(stop).is_err() {
                continue;
            }
            let closed = reader.closer();
            let conn = Conn { reader: Box::new(reader), writer: Box::new(link.writer()), close: Box::new(move || closed()), ready: Box::new(|| {}), via: "USB accessory" };
            if tx.send(conn).is_err() {
                return;
            }
            // Wait for that session to end before opening the accessory again.
            thread::sleep(Duration::from_millis(500));
            while busy.load(Ordering::Relaxed) {
                thread::sleep(Duration::from_millis(50));
            }
            last = Some(link);
        }
    });
    rx
}

fn run_session(conn: Conn, args: &Args, host: &mut dyn Host, running: &AtomicBool) -> Result<()> {
    let Conn { mut reader, writer, close, ready, via } = conn;
    let hello = protocol::read_hello(&mut reader)?;
    ready();
    println!("tablet connected over {via}");

    let (width, height) = pick_size(args, hello.width, hello.height);
    let tablet_fps = if hello.max_fps == 0 { 60 } else { hello.max_fps };
    // The panel's own rate: one Mac frame per panel refresh. A faster virtual display composites
    // changes sooner, but frames then land 1 or 2 to a refresh and motion stutters.
    let fps = args.fps.unwrap_or(tablet_fps).clamp(30, 120);
    let bitrate = args.bitrate.unwrap_or(if fps > 60 { 40 } else { 25 });
    println!(
        "tablet screen {}x{} ({} Hz) -> virtual display {width}x{height}@{fps}, {bitrate} Mbit/s",
        hello.width, hello.height, hello.max_fps
    );

    let alive = Arc::new(AtomicBool::new(true));
    let audio_on = Arc::new(AtomicBool::new(false)); // audio is off until the tablet asks
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    tx.send(protocol::config_msg(width, height, fps)).ok();

    let epoch = std::time::Instant::now();
    let timing = Arc::new(timing::Timing::new(epoch));
    let cfg = StreamConfig { width, height, fps, bitrate, epoch, timing: timing.clone(), tiles: hello.tiles };
    // Clock sync for the latency breakdown; cheap enough to run all the time.
    {
        let tx = tx.clone();
        let timing = timing.clone();
        let alive = alive.clone();
        thread::spawn(move || {
            while alive.load(Ordering::Relaxed) && tx.send(protocol::ping_msg(timing.now())).is_ok() {
                thread::sleep(Duration::from_millis(100));
            }
        });
    }
    let mut stream = match host.start(args, &cfg, audio_on.clone(), tx) {
        Ok(s) => s,
        Err(e) => {
            host.release();
            return Err(e);
        }
    };

    // writer: the only thread that writes to the socket
    {
        let mut out = writer;
        let alive = alive.clone();
        let control = stream.control.clone();
        let timing = timing.clone();
        thread::spawn(move || {
            #[cfg(target_os = "macos")]
            unsafe {
                libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE, 0);
            }
            for msg in rx {
                if out.write_all(&msg).is_err() {
                    break;
                }
                if msg[0] == protocol::MSG_VIDEO || msg[0] == protocol::MSG_TILE {
                    let pts = u64::from_be_bytes(msg[5..13].try_into().unwrap());
                    timing.written(pts, msg.len() - protocol::VIDEO_HEADER);
                }
            }
            alive.store(false, Ordering::Relaxed);
            control.close();
        });
    }
    // reader: control messages from the tablet
    {
        let input = stream.input.take();
        if input.is_none() {
            println!("touch input is not available with this capture source");
        }
        let mut inp = reader;
        let alive = alive.clone();
        let control = stream.control.clone();
        let timing = timing.clone();
        thread::spawn(move || {
            let mut input = input.map(|input| InputDecoder { input, last_down: None });
            let mut m = [0u8; 2];
            let mut payload = [0u8; 48];
            let u64_at = |p: &[u8], i: usize| u64::from_be_bytes(p[i..i + 8].try_into().unwrap());
            while inp.read_exact(&mut m).is_ok() {
                let extra = protocol::control_payload_len(m[0]);
                if inp.read_exact(&mut payload[..extra]).is_err() {
                    break;
                }
                match m[0] {
                    protocol::KIND_TIMING => timing.tablet(
                        u64_at(&payload, 0),
                        timing::TabletTimes {
                            recv_start: u64_at(&payload, 8),
                            recv_end: u64_at(&payload, 16),
                            queued: u64_at(&payload, 24),
                            decoded: u64_at(&payload, 32),
                            shown: u64_at(&payload, 40),
                        },
                    ),
                    protocol::KIND_PONG => timing.pong(u64_at(&payload, 0), u64_at(&payload, 8)),
                    protocol::KIND_POINTER | protocol::KIND_SCROLL => {
                        if let Some(i) = input.as_mut() {
                            i.handle(m[0], m[1], &payload);
                        }
                    }
                    protocol::KIND_ACK => control.ack(),
                    protocol::KIND_IDR => {
                        println!("tablet asked for a keyframe");
                        control.request_keyframe();
                    }
                    protocol::KIND_AUDIO => {
                        audio_on.store(m[1] != 0, Ordering::Relaxed);
                        println!("audio {}", if m[1] != 0 { "on" } else { "off" });
                    }
                    _ => {}
                }
            }
            alive.store(false, Ordering::Relaxed);
            control.close();
        });
    }

    println!("streaming");
    while alive.load(Ordering::Relaxed) && running.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_millis(100));
    }

    stream.control.close();
    drop(stream);
    close();
    host.release();
    println!("session ended");
    Ok(())
}

fn make_host(args: &Args) -> Result<Box<dyn Host>> {
    #[cfg(target_os = "macos")]
    {
        let _ = args;
        Ok(Box::new(mac::MacHost::new()))
    }
    #[cfg(target_os = "linux")]
    {
        Ok(Box::new(linux::LinuxHost::new(args)?))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = args;
        bail!("the host runs on macOS and Linux only");
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    let mut host = make_host(&args)?;

    let running = Arc::new(AtomicBool::new(true));
    {
        let running = running.clone();
        ctrlc::set_handler(move || running.store(false, Ordering::Relaxed))?;
    }

    let listener = TcpListener::bind(("127.0.0.1", args.port))?;
    listener.set_nonblocking(true)?;

    let busy = Arc::new(AtomicBool::new(false));
    let current = Arc::new(std::sync::Mutex::new(None));
    if !args.no_adb {
        spawn_adb_watcher(&args, busy.clone(), current.clone());
    }
    let usb = (!args.no_adb && !args.no_aoa).then(|| spawn_aoa(running.clone(), busy.clone(), current.clone()));
    println!("waiting for the tablet (plug it in with USB debugging on)...");
    let keep = Duration::from_secs(args.keep_display);

    while running.load(Ordering::Relaxed) {
        let conn = match usb.as_ref().and_then(|rx| rx.try_recv().ok()) {
            Some(c) => Some(c),
            None => match listener.accept() {
                Ok((sock, _)) => Some(Conn::tcp(sock)?),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => None,
                Err(e) => return Err(e.into()),
            },
        };
        match conn {
            Some(conn) => {
                busy.store(true, Ordering::Relaxed);
                if let Err(e) = run_session(conn, &args, host.as_mut(), &running) {
                    eprintln!("session error: {e:#}");
                }
                busy.store(false, Ordering::Relaxed);
            }
            None => {
                host.expire(keep);
                thread::sleep(Duration::from_millis(50));
            }
        }
    }
    host.shutdown();
    if !args.no_adb {
        tablet::restore(&args.adb);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct Recorder(Arc<Mutex<Vec<String>>>);

    impl Input for Recorder {
        fn pointer(&mut self, ev: Pointer, x: f64, y: f64, clicks: u32) {
            self.0.lock().unwrap().push(format!("{ev:?} {x:.2} {y:.2} x{clicks}"));
        }
        fn scroll(&mut self, dx: f64, dy: f64) {
            self.0.lock().unwrap().push(format!("scroll {dx} {dy}"));
        }
    }

    #[test]
    fn decodes_pointer_messages_and_counts_double_clicks() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut d = InputDecoder { input: Box::new(Recorder(log.clone())), last_down: None };
        let half = 32768u16.to_be_bytes();
        let p = [half[0], half[1], 0, 0];
        d.handle(protocol::KIND_POINTER, protocol::POINTER_LEFT_DOWN, &p);
        d.handle(protocol::KIND_POINTER, protocol::POINTER_LEFT_UP, &p);
        d.handle(protocol::KIND_POINTER, protocol::POINTER_LEFT_DOWN, &p);
        d.handle(protocol::KIND_POINTER, protocol::POINTER_RIGHT_DOWN, &[255, 255, 255, 255]);
        d.handle(protocol::KIND_SCROLL, 0, &[0xff, 0xf6, 0, 20]);
        assert_eq!(
            *log.lock().unwrap(),
            vec![
                "Down(Left) 0.50 0.00 x1",
                "Up(Left) 0.50 0.00 x1",
                "Down(Left) 0.50 0.00 x2",
                "Down(Right) 1.00 1.00 x2",
                "scroll -10 20",
            ]
        );
    }
}
