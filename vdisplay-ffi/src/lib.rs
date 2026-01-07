#![allow(unexpected_cfgs)]
use objc::{
    msg_send,
    runtime::{BOOL, Class, NO, Object, YES},
    sel, sel_impl,
};
use std::ffi::{CString, c_char};

#[repr(C)]
#[derive(Debug)]
pub struct DisplayObject {
    pub display_id: u32,
    pub width: u32,
    pub height: u32,
}

#[cfg(target_os = "macos")]
#[link(name = "vdisplay")]
#[allow(dead_code)]
unsafe extern "C" {
    fn load();
}

#[cfg(target_os = "macos")]
pub struct VDisplay {
    obj: *mut Object,
}

#[cfg(target_os = "macos")]
impl VDisplay {
    pub fn new() -> Self {
        unsafe {
            load();
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

#[cfg(not(target_os = "macos"))]
pub struct VDisplay {}

#[cfg(not(target_os = "macos"))]
impl VDisplay {
    pub fn new() -> Self {
        panic!("VDisplay is only availbale on macos");
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
        panic!("VDisplay is only availbale on macos");
    }

    pub fn clone_virtual_display(&mut self, display_name: &str, use_mirror: bool) -> DisplayObject {
        panic!("VDisplay is only availbale on macos");
    }

    pub fn destroy_virtual_display(&mut self) -> bool {
        panic!("VDisplay is only availbale on macos");
    }
}
