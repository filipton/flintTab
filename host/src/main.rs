//! Turns an Android tablet (connected over USB) into a secondary macOS display.
//!
//! Flow: `adb reverse` exposes this host's TCP port inside the tablet, the tablet app
//! connects and reports its screen size, we create a virtual display of that size,
//! capture it, encode it to H.264 and stream it back over the same socket.

#[cfg(target_os = "macos")]
mod capture;
mod gate;
mod protocol;
mod sps;
#[cfg(target_os = "macos")]
mod vt;

use anyhow::{Result, bail};
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
    time::{Duration, Instant},
};

const APP_ACTIVITY: &str = "dev.tabdisplay/.MainActivity";

#[derive(Parser)]
#[command(about = "Use an Android tablet as a USB secondary display for macOS")]
struct Args {
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

/// Owns the virtual display and keeps it alive for a while after the tablet goes away,
/// so a quick reconnect reuses it and macOS does not shuffle windows back to the main screen.
#[cfg(target_os = "macos")]
struct Displays {
    vd: vdisplay_ffi::VDisplay,
    current: Option<((u32, u32, u32), u32)>, // (w, h, fps), display id
    idle_since: Option<Instant>,
}

#[cfg(target_os = "macos")]
impl Displays {
    fn get(&mut self, args: &Args, w: u32, h: u32, fps: u32) -> Result<u32> {
        self.idle_since = None;
        if let Some((mode, id)) = self.current {
            if mode == (w, h, fps) {
                println!("reusing the virtual display");
                return Ok(id);
            }
            self.destroy();
        }
        let d = self.vd.create_virtual_display(w, h, fps as f64, !args.no_hidpi, "Tablet", args.ppi, false);
        if d.display_id == 0 {
            bail!("failed to create the virtual display");
        }
        self.current = Some(((w, h, fps), d.display_id));
        Ok(d.display_id)
    }

    fn release(&mut self) {
        self.idle_since = Some(Instant::now());
    }

    fn expire(&mut self, after: Duration) {
        if self.idle_since.is_some_and(|t| t.elapsed() >= after) {
            self.destroy();
        }
    }

    fn destroy(&mut self) {
        if self.current.take().is_some() {
            self.vd.destroy_virtual_display();
            println!("virtual display removed");
        }
        self.idle_since = None;
    }
}

#[cfg(target_os = "macos")]
fn run_session(mut sock: TcpStream, args: &Args, displays: &mut Displays, running: &AtomicBool) -> Result<()> {
    use screencapturekit::CVPixelBuffer;

    sock.set_nodelay(true)?;
    sock.set_read_timeout(Some(Duration::from_secs(5)))?;
    let hello = protocol::read_hello(&mut sock)?;
    sock.set_read_timeout(None)?;

    let (w, h) = pick_size(args, hello.width, hello.height);
    let tablet_fps = if hello.max_fps == 0 { 60 } else { hello.max_fps };
    let fps = args.fps.unwrap_or(tablet_fps).clamp(30, 120);
    let bitrate = args.bitrate.unwrap_or(if fps > 60 { 40 } else { 25 });
    println!(
        "tablet screen {}x{} ({} Hz) -> virtual display {w}x{h}@{fps}, {bitrate} Mbit/s",
        hello.width, hello.height, hello.max_fps
    );

    let display_id = displays.get(args, w, h, fps)?;

    let alive = Arc::new(AtomicBool::new(true));
    let audio_on = Arc::new(AtomicBool::new(false)); // audio is off until the tablet asks
    let gate = Arc::new(gate::Gate::<CVPixelBuffer>::new());
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    tx.send(protocol::config_msg(w, h, fps)).ok();

    // writer: the only thread that writes to the socket
    {
        let mut out = sock.try_clone()?;
        let alive = alive.clone();
        let gate = gate.clone();
        thread::spawn(move || {
            unsafe {
                libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE, 0);
            }
            for msg in rx {
                if out.write_all(&msg).is_err() {
                    break;
                }
            }
            alive.store(false, Ordering::Relaxed);
            gate.close();
        });
    }
    // reader: control messages from the tablet
    {
        let mut inp = sock.try_clone()?;
        let alive = alive.clone();
        let audio_on = audio_on.clone();
        let gate = gate.clone();
        thread::spawn(move || {
            let mut m = [0u8; 2];
            while inp.read_exact(&mut m).is_ok() {
                match m[0] {
                    protocol::KIND_ACK => gate.ack(),
                    protocol::KIND_IDR => {
                        println!("tablet asked for a keyframe");
                        gate.request_keyframe();
                    }
                    protocol::KIND_AUDIO => {
                        audio_on.store(m[1] != 0, Ordering::Relaxed);
                        println!("audio {}", if m[1] != 0 { "on" } else { "off" });
                    }
                    _ => {}
                }
            }
            alive.store(false, Ordering::Relaxed);
            gate.close();
        });
    }

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

    let tx_audio = tx.clone();
    let capture_gate = gate.clone();
    let capture = capture::Capture::start(
        display_id,
        w,
        h,
        fps,
        move |pixel_buffer| capture_gate.push(pixel_buffer),
        move |pcm| {
            if audio_on.load(Ordering::Relaxed) {
                tx_audio.send(protocol::audio_msg(pcm)).ok();
            }
        },
    );

    let result = capture.map(|capture| {
        println!("streaming");
        while alive.load(Ordering::Relaxed) && running.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(100));
        }
        drop(capture); // no more frames after this
    });

    gate.close();
    let _ = encode_thread.join();
    let _ = sock.shutdown(Shutdown::Both);
    displays.release();
    println!("session ended");
    result
}

#[cfg(target_os = "macos")]
fn serve(args: Args) -> Result<()> {
    let running = Arc::new(AtomicBool::new(true));
    {
        let running = running.clone();
        ctrlc::set_handler(move || running.store(false, Ordering::Relaxed))?;
    }

    let listener = TcpListener::bind(("127.0.0.1", args.port))?;
    listener.set_nonblocking(true)?;

    let busy = Arc::new(AtomicBool::new(false));
    spawn_adb_watcher(&args, busy.clone());
    println!("waiting for the tablet (USB debugging on, app installed)...");

    let mut displays = Displays { vd: vdisplay_ffi::VDisplay::new(), current: None, idle_since: None };
    let keep = Duration::from_secs(args.keep_display);

    while running.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((sock, _)) => {
                sock.set_nonblocking(false)?;
                busy.store(true, Ordering::Relaxed);
                if let Err(e) = run_session(sock, &args, &mut displays, &running) {
                    eprintln!("session error: {e:#}");
                    displays.release();
                }
                busy.store(false, Ordering::Relaxed);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                displays.expire(keep);
                thread::sleep(Duration::from_millis(100));
            }
            Err(e) => return Err(e.into()),
        }
    }
    displays.destroy();
    Ok(())
}

fn main() -> Result<()> {
    let args = Args::parse();
    #[cfg(target_os = "macos")]
    return serve(args);
    #[cfg(not(target_os = "macos"))]
    {
        let _ = args;
        bail!("the host only runs on macOS (virtual display + ScreenCaptureKit)");
    }
}
