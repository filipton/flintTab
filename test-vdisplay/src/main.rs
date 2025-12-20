use anyhow::Result;
use std::time::Duration;
use xcap::Monitor;

fn main() -> Result<()> {
    vdisplay_ffi::init_virtual_display();
    let vd = vdisplay_ffi::create_virtual_display(1920, 1080, 30.0, true, "Test", 300, false);
    println!("{vd:?}");

    let monitor = Monitor::from_point(2000, 100).unwrap();
    println!("{monitor:?}");
    let (video_recorder, sx) = monitor.video_recorder().unwrap();

    std::thread::spawn(move || {
        loop {
            match sx.recv() {
                Ok(frame) => {
                    println!("frame: {:?}", frame.width);
                }
                _ => continue,
            }
        }
    });

    println!("start");
    video_recorder.start().unwrap();
    std::thread::sleep(Duration::from_secs(2));
    println!("stop");
    video_recorder.stop().unwrap();
    std::thread::sleep(Duration::from_secs(2));
    println!("start");
    video_recorder.start().unwrap();
    std::thread::sleep(Duration::from_secs(2));
    println!("stop");
    video_recorder.stop().unwrap();

    std::thread::sleep(Duration::from_secs(60));
    let r = vdisplay_ffi::destroy_virtual_display();
    println!("r {r}");
    vdisplay_ffi::init_virtual_cleanup();
    Ok(())
}
