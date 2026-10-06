//! ScreenCaptureKit capture of one display (NV12 IOSurface buffers) plus system audio (i16 PCM).

use anyhow::{Result, bail};
use screencapturekit::{
    CMSampleBuffer, CMTime, CVPixelBuffer,
    prelude::{
        PixelFormat, SCContentFilter, SCShareableContent, SCStreamConfiguration, SCStreamOutputType,
    },
    stream::{SCStream, SCStreamDelegate, SCStreamOutput},
};
use std::{
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

/// macOS stopped one of this process's captures on its own (not on request). Starting a capture
/// in any process can then hang until this one exits (seen: every new start on the Mac timed
/// out until the process holding the stopped stream was gone), so the host starts afresh.
pub static STOPPED_BY_SYSTEM: AtomicBool = AtomicBool::new(false);

struct StopWatch;

impl SCStreamDelegate for StopWatch {
    fn did_stop_with_error(&self, error: screencapturekit::error::SCError) {
        eprintln!("screen capture stopped by macOS: {error}");
        STOPPED_BY_SYSTEM.store(true, Ordering::Relaxed);
    }
}

/// `f()` on a thread of its own, given up after `secs` (ScreenCaptureKit calls can go
/// unanswered; the host must not hang on them).
fn within<T: Send + 'static>(secs: u64, what: &str, f: impl FnOnce() -> T + Send + 'static) -> Result<T> {
    let (tx, rx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(Duration::from_secs(secs))
        .map_err(|_| anyhow::anyhow!("ScreenCaptureKit did not answer ({what}) within {secs} s; trying again"))
}

use crate::protocol::{AUDIO_CHANNELS, AUDIO_RATE};

unsafe extern "C" {
    fn mach_absolute_time() -> u64;
    fn mach_timebase_info(info: *mut u32) -> i32;
}

/// What changed in a frame: rectangles x0, y0, x1, y1 in pixels, or `None` if unknown
/// (treat as everything).
pub type Dirty = Option<Vec<[f64; 4]>>;

type VideoSink = Box<dyn FnMut(CVPixelBuffer, Option<Duration>, Dirty) + Send>;
type AudioSink = Box<dyn FnMut(&[u8]) + Send>;

struct VideoHandler {
    sink: Mutex<VideoSink>,
}

struct AudioHandler {
    sink: Mutex<AudioSink>,
}

impl SCStreamOutput for VideoHandler {
    fn did_output_sample_buffer(&self, sample: CMSampleBuffer, _t: SCStreamOutputType) {
        // Status-only samples (idle/blank frames) carry no image.
        let Some(pixel_buffer) = sample.image_buffer() else { return };
        // How long ago the compositor finished this frame (display time is mach absolute time).
        let age = sample.display_time().map(|shown| {
            let mut tb = [0u32; 2]; // numer, denom
            let now = unsafe {
                mach_timebase_info(tb.as_mut_ptr());
                mach_absolute_time()
            };
            Duration::from_nanos(now.saturating_sub(shown) * tb[0] as u64 / tb[1].max(1) as u64)
        });
        // The areas that changed since the previous frame.
        let dirty: Dirty = sample.dirty_rects().map(|rects| {
            rects.iter().filter(|r| r.width > 0.0 && r.height > 0.0).map(|r| [r.x, r.y, r.x + r.width, r.y + r.height]).collect()
        });
        if std::env::var_os("TD_DEBUG_DIRTY").is_some()
            && let Some(d) = &dirty
        {
            let max = d.iter().fold([0f64; 2], |m: [f64; 2], r: &[f64; 4]| [m[0].max(r[2]), m[1].max(r[3])]);
            eprintln!("dirty: {} rects, reaching {:.0}x{:.0}; frame {}x{}", d.len(), max[0], max[1], pixel_buffer.width(), pixel_buffer.height());
        }
        // Zero-copy: the IOSurface-backed NV12 buffer (retained) goes straight to the encoder.
        (self.sink.lock().unwrap())(pixel_buffer, age, dirty);
    }
}

impl SCStreamOutput for AudioHandler {
    fn did_output_sample_buffer(&self, sample: CMSampleBuffer, _t: SCStreamOutputType) {
        let Some(list) = sample.audio_buffer_list() else { return };
        let f32s = |b: &[u8]| -> Vec<f32> {
            b.chunks_exact(4).map(|c| f32::from_le_bytes(c.try_into().unwrap())).collect()
        };
        let to_i16 = |v: f32| (v.clamp(-1.0, 1.0) * 32767.0) as i16;

        let mut pcm: Vec<u8> = Vec::new();
        match list.num_buffers() {
            0 => return,
            1 => {
                // Already interleaved (or mono).
                let b = list.get(0).unwrap();
                let s = f32s(b.data());
                if b.number_channels == 1 {
                    for v in s {
                        let x = to_i16(v).to_le_bytes();
                        pcm.extend_from_slice(&x);
                        pcm.extend_from_slice(&x);
                    }
                } else {
                    for v in s {
                        pcm.extend_from_slice(&to_i16(v).to_le_bytes());
                    }
                }
            }
            _ => {
                // Planar: one buffer per channel.
                let l = f32s(list.get(0).unwrap().data());
                let r = f32s(list.get(1).unwrap().data());
                for (l, r) in l.iter().zip(r.iter()) {
                    pcm.extend_from_slice(&to_i16(*l).to_le_bytes());
                    pcm.extend_from_slice(&to_i16(*r).to_le_bytes());
                }
            }
        }
        (self.sink.lock().unwrap())(&pcm);
    }
}

pub struct Capture {
    stream: SCStream,
}

impl Capture {
    pub fn start(
        display_id: u32,
        width: u32,
        height: u32,
        fps: u32,
        shows_cursor: bool,
        // Only when the tablet plays the sound: audio capture is one more system service the
        // start waits on (start hangs after a quick restart were seen with it always on).
        with_audio: bool,
        video: impl FnMut(CVPixelBuffer, Option<Duration>, Dirty) + Send + 'static,
        audio: impl FnMut(&[u8]) + Send + 'static,
    ) -> Result<Self> {
        // The virtual display needs a moment before ScreenCaptureKit lists it.
        let mut display = None;
        for _ in 0..50 {
            let content = within(5, "listing displays", SCShareableContent::get)??;
            display = content.displays().iter().find(|d| d.display_id() == display_id).cloned();
            if display.is_some() {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
        let Some(display) = display else {
            bail!("virtual display {display_id} never showed up in ScreenCaptureKit \
                   (is Screen Recording permission granted to this terminal?)");
        };

        let filter = SCContentFilter::create()
            .with_display(&display)
            .with_excluding_windows(&[])
            .build();

        let config = SCStreamConfiguration::new()
            .with_width(width)
            .with_height(height)
            .with_pixel_format(PixelFormat::YCbCr_420v)
            .with_minimum_frame_interval(&CMTime::new(1, fps as i32))
            // The encoder keeps the newest frame to re-encode it while the screen is idle,
            // so leave ScreenCaptureKit enough surfaces not to stall on that one.
            .with_queue_depth(5)
            .with_shows_cursor(shows_cursor)
            .with_captures_audio(with_audio)
            .with_sample_rate(AUDIO_RATE as i32)
            .with_channel_count(AUDIO_CHANNELS as i32)
            .with_excludes_current_process_audio(true);

        let mut stream = SCStream::new_with_delegate(&filter, &config, StopWatch);
        stream.add_output_handler(
            VideoHandler { sink: Mutex::new(Box::new(video)) },
            SCStreamOutputType::Screen,
        );
        if with_audio {
            stream.add_output_handler(AudioHandler { sink: Mutex::new(Box::new(audio)) }, SCStreamOutputType::Audio);
        }
        // ScreenCaptureKit can leave start_capture unanswered (seen while another virtual display
        // was being removed and created): wait a few seconds, then give up on this session; the
        // tablet reconnects and capture starts afresh, instead of the host hanging for good.
        struct Pending(SCStream);
        unsafe impl Send for Pending {}
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let pending = Pending(stream);
        thread::spawn(move || {
            let p = pending;
            let r = p.0.start_capture().map_err(|e| anyhow::anyhow!("{e:?}"));
            let started = r.is_ok();
            if let Err(std::sync::mpsc::SendError((p, _))) = done_tx.send((p, r)) {
                // Answered after the session gave up on it: nobody owns this capture, which
                // would keep running (and capturing) until the display goes away.
                if started {
                    let _ = p.0.stop_capture();
                }
            }
        });
        match done_rx.recv_timeout(Duration::from_secs(5)) {
            Ok((p, Ok(()))) => Ok(Self { stream: p.0 }),
            Ok((_, Err(e))) => Err(e.context("starting screen capture")),
            Err(_) => bail!("ScreenCaptureKit did not start capturing within 5 s; trying again"),
        }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let _ = self.stream.stop_capture();
    }
}
