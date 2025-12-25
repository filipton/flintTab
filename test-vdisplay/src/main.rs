use anyhow::Result;
use image::{ImageBuffer, Rgba};
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use screencapturekit::{
    async_api::{AsyncSCShareableContent, AsyncSCStream},
    prelude::{SCContentFilter, SCStreamConfiguration, SCStreamOutputType},
    recording_output::{
        SCRecordingOutput, SCRecordingOutputCodec, SCRecordingOutputConfiguration,
        SCRecordingOutputFileType,
    },
};

#[tokio::main]
async fn main() -> Result<()> {
    vdisplay_ffi::init_virtual_display();
    let vd = vdisplay_ffi::create_virtual_display(1920, 1080, 30.0, true, "Test", 300, false);
    println!("{vd:?}");

    /*
    let output_path = PathBuf::from("/tmp/screen_recording.mp4");
    let config = SCRecordingOutputConfiguration::new()
        .with_output_url(&output_path)
        .with_video_codec(SCRecordingOutputCodec::H264)
        .with_output_file_type(SCRecordingOutputFileType::MP4);
    */

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
        .with_width(vd.width)
        .with_height(vd.height);

    let stream = AsyncSCStream::new(&filter, &config, 30, SCStreamOutputType::Screen);
    stream.start_capture()?;

    for _ in 0..10 {
        if let Some(frame) = stream.next().await {
            println!("Got frame! {}", frame.num_samples());
        }
    }

    stream.stop_capture()?;

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
