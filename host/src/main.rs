//! Turns an Android tablet (connected over USB) into a secondary display for macOS or Linux.
//!
//! Flow: `adb reverse` exposes this host's TCP port inside the tablet, the tablet app
//! connects and reports its screen size, we create a virtual display of that size,
//! capture it, encode it to H.264 and stream it back over the same socket.

#[cfg(target_os = "macos")]
mod capture;
mod gate;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod mac;
mod protocol;
mod sps;
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
    /// Do not start the tablet app automatically
    #[arg(long)]
    no_launch: bool,
    /// Do not run adb at all (the tablet connects some other way, e.g. a test client)
    #[arg(long, hide = true)]
    no_adb: bool,
    /// Linux: stream a GStreamer test pattern instead of a screen (for testing without a desktop)
    #[arg(long, hide = true)]
    test_source: bool,
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

/// Keeps `adb reverse` alive (it is dropped on unplug) and opens the app on connect.
fn spawn_adb_watcher(args: &Args, busy: Arc<AtomicBool>) {
    let adb = args.adb.clone();
    let port = args.port;
    let launch = !args.no_launch;
    thread::spawn(move || {
        let mut was_ok = false;
        loop {
            if !busy.load(Ordering::Relaxed) {
                let spec = format!("tcp:{port}");
                let ok = Command::new(&adb)
                    .args(["reverse", &spec, &spec])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false);
                if ok && !was_ok {
                    println!("tablet detected over USB");
                    if launch {
                        let _ = Command::new(&adb)
                            .args(["shell", "am", "start", "-n", APP_ACTIVITY])
                            .stdout(Stdio::null())
                            .stderr(Stdio::null())
                            .status();
                    }
                }
                was_ok = ok;
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
}

/// A running capture + encode pipeline; dropping `guard` stops it.
pub struct Stream {
    pub control: Arc<dyn Control>,
    pub guard: Box<dyn std::any::Any>,
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

fn run_session(mut sock: TcpStream, args: &Args, host: &mut dyn Host, running: &AtomicBool) -> Result<()> {
    sock.set_nodelay(true)?;
    sock.set_read_timeout(Some(Duration::from_secs(5)))?;
    let hello = protocol::read_hello(&mut sock)?;
    sock.set_read_timeout(None)?;

    let (width, height) = pick_size(args, hello.width, hello.height);
    let tablet_fps = if hello.max_fps == 0 { 60 } else { hello.max_fps };
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

    let cfg = StreamConfig { width, height, fps, bitrate };
    let stream = match host.start(args, &cfg, audio_on.clone(), tx) {
        Ok(s) => s,
        Err(e) => {
            host.release();
            return Err(e);
        }
    };

    // writer: the only thread that writes to the socket
    {
        let mut out = sock.try_clone()?;
        let alive = alive.clone();
        let control = stream.control.clone();
        thread::spawn(move || {
            #[cfg(target_os = "macos")]
            unsafe {
                libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE, 0);
            }
            for msg in rx {
                if out.write_all(&msg).is_err() {
                    break;
                }
            }
            alive.store(false, Ordering::Relaxed);
            control.close();
        });
    }
    // reader: control messages from the tablet
    {
        let mut inp = sock.try_clone()?;
        let alive = alive.clone();
        let control = stream.control.clone();
        thread::spawn(move || {
            let mut m = [0u8; 2];
            while inp.read_exact(&mut m).is_ok() {
                match m[0] {
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
    let _ = sock.shutdown(Shutdown::Both);
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
    if !args.no_adb {
        spawn_adb_watcher(&args, busy.clone());
    }
    println!("waiting for the tablet (USB debugging on, app installed)...");
    let keep = Duration::from_secs(args.keep_display);

    while running.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((sock, _)) => {
                sock.set_nonblocking(false)?;
                busy.store(true, Ordering::Relaxed);
                if let Err(e) = run_session(sock, &args, host.as_mut(), &running) {
                    eprintln!("session error: {e:#}");
                }
                busy.store(false, Ordering::Relaxed);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                host.expire(keep);
                thread::sleep(Duration::from_millis(100));
            }
            Err(e) => return Err(e.into()),
        }
    }
    host.shutdown();
    Ok(())
}
