use std::ffi::CString;

#[repr(C)]
#[derive(Debug)]
pub struct DisplayObject {
    pub display_id: u32,
    pub width: u32,
    pub height: u32,
}

#[cfg(target_os = "macos")]
#[link(name = "vdisplay")]
unsafe extern "C" {
    fn VDisplay_Init();
    fn VDisplay_CreateVirtualDisplay(
        width: u32,
        height: u32,
        refresh_rate: f64,
        hi_dpi: bool,
        display_name_str: *const std::os::raw::c_char,
        ppi: i32,
        use_mirror: bool,
    ) -> DisplayObject;
    fn VDisplay_CloneVirtualDisplay(
        display_name_str: *const std::os::raw::c_char,
        use_mirror: bool,
    ) -> DisplayObject;
    fn VDisplay_DestroyVirtualDisplay() -> bool;
    fn VDisplay_Cleanup();
}

pub fn init_virtual_display() {
    unsafe { VDisplay_Init() }
}

pub fn init_virtual_cleanup() {
    unsafe { VDisplay_Cleanup() }
}

pub fn create_virtual_display(
    width: u32,
    height: u32,
    refresh_rate: f64,
    hi_dpi: bool,
    display_name: &str,
    ppi: i32,
    use_mirror: bool,
) -> DisplayObject {
    let c_name = CString::new(display_name).expect("CString creation failed");

    unsafe {
        VDisplay_CreateVirtualDisplay(
            width,
            height,
            refresh_rate,
            hi_dpi,
            c_name.as_ptr(),
            ppi,
            use_mirror,
        )
    }
}

pub fn clone_virtual_display(display_name: &str, use_mirror: bool) -> DisplayObject {
    let c_name = CString::new(display_name).expect("CString creation failed");
    unsafe { VDisplay_CloneVirtualDisplay(c_name.as_ptr(), use_mirror) }
}

pub fn destroy_virtual_display() -> bool {
    unsafe { VDisplay_DestroyVirtualDisplay() }
}
