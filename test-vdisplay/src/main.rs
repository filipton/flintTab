use anyhow::Result;
use image::{ImageBuffer, Rgba};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use xcap::Monitor;

fn main() -> Result<()> {
    vdisplay_ffi::init_virtual_display();
    let vd = vdisplay_ffi::create_virtual_display(1920, 1080, 30.0, true, "Test", 300, false);
    println!("{vd:?}");

    let Some(monitor) = Monitor::all()?
        .iter()
        .filter(|m| m.id().unwrap_or(u32::max_value()) == vd.display_id)
        .collect::<Vec<_>>()
        .first()
        .cloned()
        .cloned()
    else {
        println!("Monitor not found!");
        return Ok(());
    };

    println!("{monitor:?}");
    let (video_recorder, sx) = monitor.video_recorder().unwrap();

    std::thread::spawn(move || {
        loop {
            match sx.recv() {
                Ok(frame) => {
                    println!("frame: {:?} {}", frame.width, frame.raw.len());
                    // frame raw is rgba
                    save_webp(
                        &PathBuf::from("/tmp/test.webp"),
                        &frame.raw,
                        1920,
                        1080,
                        80.0,
                    )
                    .unwrap();
                }
                _ => continue,
            }
        }
    });

    video_recorder.start().unwrap();

    std::thread::sleep(Duration::from_secs(500));
    video_recorder.stop().unwrap();
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
