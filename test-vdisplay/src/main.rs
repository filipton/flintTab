use anyhow::Result;
use std::{
    io::Write,
    path::{Path, PathBuf},
    process::{ChildStdin, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use screencapturekit::{
    CMSampleBuffer, CMTime,
    cv::CVPixelBufferLockFlags,
    prelude::{
        PixelFormat, SCContentFilter, SCShareableContent, SCStreamConfiguration, SCStreamOutputType,
    },
    stream::{SCStream, SCStreamOutput},
};

struct FrameHandler {
    count: Arc<AtomicUsize>,
    ffmpeg_stdin: Arc<Mutex<ChildStdin>>,
    last_time: Arc<Mutex<Instant>>,
}

impl SCStreamOutput for FrameHandler {
    fn did_output_sample_buffer(&self, sample: CMSampleBuffer, _type: SCStreamOutputType) {
        let Some(pixel_buffer) = sample.image_buffer() else {
            return;
        };

        let Ok(guard) = pixel_buffer.lock(CVPixelBufferLockFlags::READ_ONLY) else {
            return;
        };

        let data = guard.as_slice();

        {
            let mut stdin = self.ffmpeg_stdin.lock().unwrap();
            stdin.write_all(data).unwrap();
        }

        let n = self.count.fetch_add(1, Ordering::Relaxed);
        if n % 30 == 0 {
            let mut last_time = self.last_time.lock().unwrap();
            println!(
                "📹 Frame {n} | {}",
                1000.0 / last_time.elapsed().as_millis() as f32
            );

            *last_time = Instant::now();
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    /*
        vdisplay_ffi::init_virtual_display();
        let vd = vdisplay_ffi::create_virtual_display(1920, 1080, 60.0, true, "Test", 300, false);
        println!("{vd:?}");

    */
    let content = SCShareableContent::get()?;
    let Some(display) = &content
        .displays()
        .iter()
        //.filter(|d| d.display_id() == vd.display_id)
        .cloned()
        .next()
    else {
        println!("[ERROR] No display found");
        return Ok(());
    };

    let filter = SCContentFilter::create()
        .with_display(display)
        .with_excluding_windows(&[])
        .build();

    let config = SCStreamConfiguration::new()
        .with_width(1920)
        .with_height(1080)
        .with_pixel_format(PixelFormat::BGRA)
        .with_minimum_frame_interval(&CMTime::new(1, 60));

    let mut stream = SCStream::new(&filter, &config);

    let mut ffmpeg = Command::new("ffmpeg")
        .arg("-f")
        .arg("rawvideo")
        .arg("-pixel_format")
        .arg("bgra")
        .arg("-video_size")
        .arg("1920x1080")
        .arg("-framerate")
        .arg("60")
        .arg("-i")
        .arg("-")
        .arg("-c:v")
        .arg("libx264")
        //.arg("-c:v")
        //.arg("h264_videotoolbox")
        //.arg("-realtime")
        //.arg("true") // Key: real-time mode
        //.arg("-prio_speed")
        //.arg("true") // Prioritize speed (lower delay)
        .arg("-preset")
        .arg("ultrafast")
        .arg("-tune")
        .arg("zerolatency")
        .arg("-bf")
        .arg("0")
        .arg("-g")
        .arg("30")
        .arg("-keyint_min")
        .arg("30")
        //.arg("-b:v")
        //.arg("50M")
        .arg("-f")
        .arg("mpegts")
        .arg("udp://192.168.1.38:1234?pkt_size=1316")
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()?;

    let stdin = ffmpeg.stdin.take().expect("Failed to open stdin");

    let count = Arc::new(AtomicUsize::new(0));
    let handler = FrameHandler {
        count: count.clone(),
        ffmpeg_stdin: Arc::new(Mutex::new(stdin)),
        last_time: Arc::new(Mutex::new(Instant::now())),
    };
    stream.add_output_handler(handler, SCStreamOutputType::Screen);
    stream.start_capture()?;

    println!("Capturing and streaming... press ctrl-c to stop");
    tokio::signal::ctrl_c().await?;

    stream.stop_capture()?;

    _ = ffmpeg.kill();
    let r = vdisplay_ffi::destroy_virtual_display();
    println!("r {r}");
    vdisplay_ffi::init_virtual_cleanup();
    Ok(())
}

pub fn save_webp(
    path: &Path,
    rgba_data: &[u8],
    width: u32,
    height: u32,
    quality: f32,
) -> Result<(), Box<dyn std::error::Error>> {
    assert_eq!(rgba_data.len(), (width * height * 4) as usize);

    let encoder = webp::Encoder::from_rgba(rgba_data, width, height);
    let webp_data = encoder.encode(quality);

    std::fs::write(path, &*webp_data)?;

    Ok(())
}
