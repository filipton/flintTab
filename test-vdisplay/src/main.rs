use anyhow::Result;
use image::{ImageBuffer, Rgba};
use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};
use tokio::io::AsyncWriteExt;

use screencapturekit::{
    CMTime,
    async_api::{AsyncSCShareableContent, AsyncSCStream},
    cv::CVPixelBufferLockFlags,
    prelude::{PixelFormat, SCContentFilter, SCStreamConfiguration, SCStreamOutputType},
    recording_output::{
        RecordingCallbacks, SCRecordingOutput, SCRecordingOutputCodec,
        SCRecordingOutputConfiguration, SCRecordingOutputFileType,
    },
    stream::SCStream,
};

#[tokio::main]
async fn main() -> Result<()> {
    vdisplay_ffi::init_virtual_display();
    let vd = vdisplay_ffi::create_virtual_display(1920, 1080, 30.0, true, "Test", 300, false);
    println!("{vd:?}");

    let content = AsyncSCShareableContent::get().await?;
    let Some(display) = &content
        .displays()
        .iter()
        .filter(|d| d.display_id() == vd.display_id)
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
        .with_minimum_frame_interval(&CMTime::new(1, 30));

    let stream = AsyncSCStream::new(&filter, &config, 30, SCStreamOutputType::Screen);

    // Stream target - change this IP to your receiving machine
    let target_host = "127.0.0.1"; // localhost for same machine, or "192.168.1.100" for different machine
    let target_port = 8554;

    println!("🎯 Streaming target: {}:{}", target_host, target_port);
    println!("💡 On the receiving machine, run:");
    println!(
        "   ffplay -fflags nobuffer -flags low_delay -framedrop tcp://{}:{}?listen",
        target_host, target_port
    );
    println!("\nWaiting 3 seconds for you to start ffplay...");
    tokio::time::sleep(Duration::from_secs(3)).await;

    // FFmpeg streams directly to the target via TCP
    let mut ffmpeg = Command::new("ffmpeg")
        .args(&[
            "-f",
            "rawvideo",
            "-pixel_format",
            "bgra",
            "-video_size",
            "1920x1080",
            "-framerate",
            "30",
            "-i",
            "pipe:0",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-tune",
            "zerolatency",
            "-g",
            "30",
            "-bf",
            "0",
            "-pix_fmt",
            "yuv420p",
            "-f",
            "mpegts", // MPEG-TS is better for streaming than raw h264
            &format!("tcp://{}:{}", target_host, target_port),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    let mut ffmpeg_stdin = tokio::process::ChildStdin::from_std(ffmpeg.stdin.take().unwrap())?;

    stream.start_capture()?;

    println!("🔴 Capturing and streaming...");

    let mut frame_count = 0u64;
    let start_time = tokio::time::Instant::now();

    // Stream for 30 seconds (or until interrupted)
    while start_time.elapsed() < Duration::from_secs(30) {
        if let Some(frame) = stream.next().await {
            if let Some(image_buffer) = frame.image_buffer() {
                let lock_guard = image_buffer
                    .lock(CVPixelBufferLockFlags::READ_ONLY)
                    .unwrap();

                let base_address = lock_guard.base_address();
                let bytes_per_row = lock_guard.bytes_per_row();
                let height = lock_guard.height();
                let frame_size = (height * bytes_per_row) as usize;

                let frame_data = unsafe { std::slice::from_raw_parts(base_address, frame_size) };

                if let Err(e) = ffmpeg_stdin.write_all(frame_data).await {
                    eprintln!("❌ Failed to write frame: {}", e);
                    eprintln!("   Is ffplay running and listening?");
                    break;
                }

                frame_count += 1;
                if frame_count % 30 == 0 {
                    let elapsed = start_time.elapsed().as_secs_f32();
                    let fps = frame_count as f32 / elapsed;
                    println!(
                        "📊 {}s | {} frames | {:.1} fps",
                        elapsed as u32, frame_count, fps
                    );
                }
            }
        }
    }

    // Cleanup
    println!("⏹️  Stopping capture...");
    stream.stop_capture()?;

    drop(ffmpeg_stdin);
    let _ = ffmpeg.kill();

    let elapsed = start_time.elapsed().as_secs_f32();
    let avg_fps = frame_count as f32 / elapsed;
    println!(
        "✅ Captured {} frames in {:.1}s ({:.1} fps)",
        frame_count, elapsed, avg_fps
    );

    std::thread::sleep(Duration::from_secs(500));
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
