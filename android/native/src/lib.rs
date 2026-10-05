//! Native hot paths of the tablet app (JNI: see `Native.kt`).

mod front;
mod neon;

use jni_sys::{JNIEnv, jbyteArray, jclass, jint, jlong, jobject};

/// Decompresses `len` bytes of LZ4 block data at `off` in the Java byte[] `src` into the direct
/// ByteBuffer `dst` (whose capacity bounds the output). Returns the bytes written, or -1.
///
/// The Kotlin version of this ran at ~180 MB/s; a 1200x800 tile took ~8 ms. This runs at
/// several GB/s and copies nothing on the Java side.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_dev_tabdisplay_Native_lz4(
    env: *mut JNIEnv,
    _class: jclass,
    src: jbyteArray,
    off: jint,
    len: jint,
    dst: jobject,
) -> jint {
    unsafe {
        let f = &**env;
        let (Some(critical), Some(release), Some(address), Some(capacity)) = (
            f.GetPrimitiveArrayCritical,
            f.ReleasePrimitiveArrayCritical,
            f.GetDirectBufferAddress,
            f.GetDirectBufferCapacity,
        ) else {
            return -1;
        };
        let out = address(env, dst) as *mut u8;
        let cap = capacity(env, dst);
        if out.is_null() || cap < 0 || off < 0 || len < 0 {
            return -1;
        }
        let input = critical(env, src, std::ptr::null_mut()) as *const u8;
        if input.is_null() {
            return -1;
        }
        let src_slice = std::slice::from_raw_parts(input.add(off as usize), len as usize);
        let dst_slice = std::slice::from_raw_parts_mut(out, cap as usize);
        let n = lz4_flex::block::decompress_into(src_slice, dst_slice).map_or(-1, |n| n as jint);
        release(env, src, input as *mut _, jni_sys::JNI_ABORT); // read only: nothing to copy back
        n
    }
}

/// Takes the front buffer (a HardwareBuffer allocated for buffer transform hint `hint`).
/// Returns a handle for the other `front*` calls, 0 on failure.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_dev_tabdisplay_Native_frontAttach(env: *mut JNIEnv, _c: jclass, hb: jobject, hint: jint) -> jlong {
    unsafe { front::Front::attach(env, hb, hint).map_or(0, |f| Box::into_raw(f) as jlong) }
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_dev_tabdisplay_Native_frontRelease(_env: *mut JNIEnv, _c: jclass, h: jlong) {
    if h != 0 {
        unsafe { drop(Box::from_raw(h as *mut front::Front)) };
    }
}

/// Fills view rectangle [x0, x1) x [y0, y1) with `rgba` (bytes R, G, B, A in memory).
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_dev_tabdisplay_Native_frontFill(
    _env: *mut JNIEnv, _c: jclass, h: jlong, x0: jint, y0: jint, x1: jint, y1: jint, rgba: jint,
) {
    let f = unsafe { &mut *(h as *mut front::Front) };
    f.fill(x0.max(0) as usize, y0.max(0) as usize, x1.max(0) as usize, y1.max(0) as usize, rgba as u32);
}

fn direct(env: *mut JNIEnv, buf: jobject) -> *mut u8 {
    unsafe { ((**env).GetDirectBufferAddress.unwrap())(env, buf) as *mut u8 }
}

/// Writes view rectangle [x0, x1) x [y0, y1) from a YUV 4:2:0 picture in direct buffers: Y at
/// `y`, U and V at `u` and `v` (`uv_step` 2 = interleaved), whose pixel (0, 0) is view pixel
/// (ox, oy). Returns false if the buffer could not be locked.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_dev_tabdisplay_Native_frontYuv(
    env: *mut JNIEnv, _c: jclass, h: jlong,
    y: jobject, y_stride: jint, u: jobject, v: jobject, uv_stride: jint, uv_step: jint,
    ox: jint, oy: jint, x0: jint, y0: jint, x1: jint, y1: jint,
) -> jni_sys::jboolean {
    let f = unsafe { &mut *(h as *mut front::Front) };
    let src = front::Yuv {
        y: direct(env, y),
        y_stride: y_stride as usize,
        u: direct(env, u),
        v: direct(env, v),
        uv_stride: uv_stride as usize,
        uv_step: uv_step as usize,
        ox: ox as usize,
        oy: oy as usize,
    };
    if src.y.is_null() || src.u.is_null() || src.v.is_null() {
        return 0;
    }
    let c = |v: jint| v.max(0) as usize;
    // Never read outside the source: the rectangle starts at or after its origin.
    let r = front::Rect { x0: c(x0).max(c(ox)), y0: c(y0).max(c(oy)), x1: c(x1), y1: c(y1) };
    f.update(&src, r) as jni_sys::jboolean
}

/// The cursor picture: premultiplied RGBA (Android's ARGB_8888 byte order) in a direct buffer.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_dev_tabdisplay_Native_frontCursorImage(
    env: *mut JNIEnv, _c: jclass, h: jlong, rgba: jobject, w: jint, hgt: jint,
) {
    let f = unsafe { &mut *(h as *mut front::Front) };
    let p = direct(env, rgba) as *const u32;
    if p.is_null() || w <= 0 || hgt <= 0 {
        return;
    }
    let px = unsafe { std::slice::from_raw_parts(p, (w * hgt) as usize) };
    f.set_cursor_image(px, w as usize, hgt as usize);
}

/// Moves the cursor's top-left to (x, y) in view pixels.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_dev_tabdisplay_Native_frontCursorMove(
    _env: *mut JNIEnv, _c: jclass, h: jlong, x: jint, y: jint, shown: jni_sys::jboolean,
) {
    let f = unsafe { &mut *(h as *mut front::Front) };
    f.move_cursor(x as i64, y as i64, shown != 0);
}

/// Copies the front buffer, in view orientation, into the direct buffer `out` (debugging).
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_dev_tabdisplay_Native_frontDump(env: *mut JNIEnv, _c: jclass, h: jlong, out: jobject) {
    let f = unsafe { &*(h as *const front::Front) };
    let p = direct(env, out) as *mut u32;
    if !p.is_null() {
        f.dump(unsafe { std::slice::from_raw_parts_mut(p, f.vw * f.vh) });
    }
}
