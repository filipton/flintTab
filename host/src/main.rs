//! Turns an Android tablet (connected over USB) into a secondary macOS display.
//!
//! Flow: `adb reverse` exposes this host's TCP port inside the tablet, the tablet app
//! connects and reports its screen size, we create a virtual display of that size,
//! capture it, encode it to H.264 and stream it back over the same socket.

mod annexb;
#[cfg(target_os = "macos")]
mod capture;
mod encoder;
mod protocol;

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
    /// Frame rate (30-60)
    #[arg(long, default_value_t = 60)]
    fps: u32,
    /// Video bitrate in Mbit/s
    #[arg(long, default_value_t = 25)]
    bitrate: u32,
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

#[cfg(target_os = "macos")]
fn run_session(
    mut sock: TcpStream,
    args: &Args,
    vd: &mut vdisplay_ffi::VDisplay,
    running: &AtomicBool,
) -> Result<()> {
    sock.set_nodelay(true)?;
    sock.set_read_timeout(Some(Duration::from_secs(5)))?;
    let hello = protocol::read_hello(&mut sock)?;
    sock.set_read_timeout(None)?;

    let (w, h) = pick_size(args, hello.width, hello.height);
    let fps = args.fps.clamp(30, 60);
    println!("tablet screen {}x{} -> virtual display {w}x{h}@{fps}", hello.width, hello.height);

    let disp = vd.create_virtual_display(w, h, fps as f64, !args.no_hidpi, "Tablet", args.ppi, false);
    if disp.display_id == 0 {
        bail!("failed to create the virtual display");
    }

    let alive = Arc::new(AtomicBool::new(true));
    let audio_on = Arc::new(AtomicBool::new(false)); // audio is off until the tablet asks
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    tx.send(protocol::config_msg(w, h, fps)).ok();

    // writer: the only thread that writes to the socket
    {
        let mut out = sock.try_clone()?;
        let alive = alive.clone();
        thread::spawn(move || {
            for msg in rx {
                if out.write_all(&msg).is_err() {
                    break;
                }
            }
            alive.store(false, Ordering::Relaxed);
        });
    }
    // reader: control messages from the tablet
    {
        let mut inp = sock.try_clone()?;
        let alive = alive.clone();
        let audio_on = audio_on.clone();
        thread::spawn(move || {
            let mut m = [0u8; 2];
            while inp.read_exact(&mut m).is_ok() {
                if m[0] == protocol::KIND_AUDIO {
                    audio_on.store(m[1] != 0, Ordering::Relaxed);
                    println!("audio {}", if m[1] != 0 { "on" } else { "off" });
                }
            }
            alive.store(false, Ordering::Relaxed);
        });
    }

    let started = Instant::now();
    let tx_video = tx.clone();
    let mut encoder = encoder::Encoder::spawn(w, h, fps, args.bitrate, move |au| {
        let pts = started.elapsed().as_micros() as u64;
        tx_video.send(protocol::video_msg(pts, &au)).ok();
    })?;
    let mut ffmpeg_in = encoder.stdin.take().unwrap();

    let alive_v = alive.clone();
    let tx_audio = tx.clone();
    let capture = capture::Capture::start(
        disp.display_id,
        w,
        h,
        fps,
        move |frame| {
            if ffmpeg_in.write_all(frame).is_err() {
                alive_v.store(false, Ordering::Relaxed);
            }
        },
        move |pcm| {
            if audio_on.load(Ordering::Relaxed) {
                tx_audio.send(protocol::audio_msg(pcm)).ok();
            }
        },
    );
    let capture = match capture {
        Ok(c) => c,
        Err(e) => {
            vd.destroy_virtual_display();
            return Err(e);
        }
    };

    println!("streaming");
    while alive.load(Ordering::Relaxed) && running.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_millis(100));
    }

    drop(capture);
    drop(encoder);
    let _ = sock.shutdown(Shutdown::Both);
    vd.destroy_virtual_display();
    println!("session ended");
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn run_session(_: TcpStream, _: &Args, _: &mut (), _: &AtomicBool) -> Result<()> {
    bail!("the host only runs on macOS");
}

fn main() -> Result<()> {
    let args = Args::parse();
    if !cfg!(target_os = "macos") {
        bail!("the host only runs on macOS (virtual display + ScreenCaptureKit)");
    }

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

    #[cfg(target_os = "macos")]
    let mut vd = vdisplay_ffi::VDisplay::new();
    #[cfg(not(target_os = "macos"))]
    let mut vd = ();

    while running.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((sock, _)) => {
                sock.set_nonblocking(false)?;
                busy.store(true, Ordering::Relaxed);
                if let Err(e) = run_session(sock, &args, &mut vd, &running) {
                    eprintln!("session error: {e:#}");
                }
                busy.store(false, Ordering::Relaxed);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(100));
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}
