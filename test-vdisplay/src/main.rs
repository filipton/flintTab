use std::time::Duration;

fn main() {
    vdisplay_ffi::init_virtual_display();
    let vd = vdisplay_ffi::create_virtual_display(1920, 1080, 30.0, true, "Test", 300, false);
    println!("{vd:?}");
    std::thread::sleep(Duration::from_secs(60));
    let r = vdisplay_ffi::destroy_virtual_display();
    println!("r {r}");
    vdisplay_ffi::init_virtual_cleanup();
}
