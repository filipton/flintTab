//! ScreenCaptureKit capture of one display (NV12 IOSurface buffers) plus system audio (i16 PCM).

use anyhow::{Result, bail};
use screencapturekit::{
    CMSampleBuffer, CMTime, CVPixelBuffer,
    prelude::{
        PixelFormat, SCContentFilter, SCShareableContent, SCStreamConfiguration, SCStreamOutputType,
    },
    stream::{SCStream, SCStreamOutput},
};
use std::{sync::Mutex, thread, time::Duration};

use crate::protocol::{AUDIO_CHANNELS, AUDIO_RATE};

type VideoSink = Box<dyn FnMut(CVPixelBuffer) + Send>;
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
        // Zero-copy: the IOSurface-backed NV12 buffer (retained) goes straight to the encoder.
        (self.sink.lock().unwrap())(pixel_buffer);
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
        video: impl FnMut(CVPixelBuffer) + Send + 'static,
        audio: impl FnMut(&[u8]) + Send + 'static,
    ) -> Result<Self> {
        // The virtual display needs a moment before ScreenCaptureKit lists it.
        let mut display = None;
        for _ in 0..50 {
            let content = SCShareableContent::get()?;
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
            .with_captures_audio(true)
            .with_sample_rate(AUDIO_RATE as i32)
            .with_channel_count(AUDIO_CHANNELS as i32)
            .with_excludes_current_process_audio(true);

        let mut stream = SCStream::new(&filter, &config);
        stream.add_output_handler(
            VideoHandler { sink: Mutex::new(Box::new(video)) },
            SCStreamOutputType::Screen,
        );
        stream.add_output_handler(
            AudioHandler { sink: Mutex::new(Box::new(audio)) },
            SCStreamOutputType::Audio,
        );
        stream.start_capture()?;
        Ok(Self { stream })
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let _ = self.stream.stop_capture();
    }
}
