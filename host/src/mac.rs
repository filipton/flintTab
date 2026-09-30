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

use crate::{Args, Control, Host, Stream, StreamConfig, capture, gate::Gate, protocol, vt};

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
        Ok(Stream { control: gate, guard: Box::new(running) })
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
