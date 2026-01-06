#![allow(unexpected_cfgs)]

use objc::{
    msg_send,
    runtime::{BOOL, Class, NO, Object, YES},
    sel, sel_impl,
};
use std::ffi::{CString, c_char};

pub struct VDisplay {
    obj: *mut Object,
}

impl VDisplay {
    pub fn new() -> Self {
        unsafe {
            let cls = Class::get("VDisplayWrapper").expect("Class not found");
            let obj: *mut Object = msg_send![cls, alloc];
            let obj: *mut Object = msg_send![obj, init];
            VDisplay { obj }
        }
    }

    pub fn create_virtual_display(
        &mut self,
        width: u32,
        height: u32,
        refresh_rate: f64,
        hi_dpi: bool,
        display_name: &str,
        ppi: i32,
        use_mirror: bool,
    ) -> DisplayObject {
        unsafe {
            let display_name_cstr = CString::new(display_name).expect("CString failed");
            let display_name_ptr: *const c_char = display_name_cstr.as_ptr();
            let hi_dpi: BOOL = if hi_dpi { YES } else { NO };
            let use_mirror: BOOL = if use_mirror { YES } else { NO };

            let res: DisplayObject = msg_send![
                self.obj,
                createVirtualDisplay:width
                height:height
                refreshRate:refresh_rate
                hiDPI:hi_dpi
                displayName:display_name_ptr
                ppi:ppi
                useMirror:use_mirror
            ];
            res
        }
    }

    pub fn clone_virtual_display(&mut self, display_name: &str, use_mirror: bool) -> DisplayObject {
        unsafe {
            let display_name_cstr = CString::new(display_name).expect("CString failed");
            let display_name_ptr: *const c_char = display_name_cstr.as_ptr();
            let use_mirror: BOOL = if use_mirror { YES } else { NO };

            let res: DisplayObject = msg_send![
                self.obj,
                cloneVirtualDisplay:display_name_ptr
                useMirror:use_mirror
            ];
            res
        }
    }

    pub fn destroy_virtual_display(&mut self) -> bool {
        unsafe {
            let result: BOOL = msg_send![self.obj, destroyVirtualDisplay];
            result == YES
        }
    }
}

/*
class VDisplay {
public:
  DisplayObject CreateVirtualDisplay(u32 width, u32 height, double refreshRate, bool hiDPI, char *displayNameStr, i32 ppi, bool useMirror);
  DisplayObject CloneVirtualDisplay(char *displayNameStr, bool useMirror);
  bool DestroyVirtualDisplay();

private:
  CGVirtualDisplay *_display;
  CGVirtualDisplayDescriptor *_descriptor;
  CGVirtualDisplaySettings *_settings;

  void InitializeDescriptor(NSString *displayName, u32 width, u32 height, i32 ppi);
  void InitializeSettings(u32 width, u32 height, CGFloat refreshRate, bool hiDPI);
  DisplayObject CreateDisplayObject(unsigned int width, unsigned int height);
  DisplayObject NullDisplayObject();
*/

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
