//! Native hot paths of the tablet app (JNI: see `Native.kt`).

mod chain;
mod front;
mod neon;
mod yuv;

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

/// Shows everything written since the last call, timed against the panel's scan.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_dev_tabdisplay_Native_frontPresent(_env: *mut JNIEnv, _c: jclass, h: jlong) -> jni_sys::jboolean {
    let f = unsafe { &mut *(h as *mut front::Front) };
    f.present_pending() as jni_sys::jboolean
}

/// The panel's scan timing: a vsync (System.nanoTime clock) and the refresh period, both ns.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_dev_tabdisplay_Native_frontVsync(_env: *mut JNIEnv, _c: jclass, h: jlong, vsync: jlong, period: jlong) {
    let f = unsafe { &mut *(h as *mut front::Front) };
    f.set_vsync(vsync, period);
}

/// Copies what the panel scans out (the front buffer itself), in view orientation (debugging).
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_dev_tabdisplay_Native_frontDumpScanout(env: *mut JNIEnv, _c: jclass, h: jlong, out: jobject) -> jni_sys::jboolean {
    let f = unsafe { &*(h as *const front::Front) };
    let p = direct(env, out) as *mut u32;
    if p.is_null() {
        return 0;
    }
    f.dump_front(unsafe { std::slice::from_raw_parts_mut(p, f.vw * f.vh) }) as jni_sys::jboolean
}

/// A frame was shown (for the smoothness log).
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_dev_tabdisplay_Native_frontCountFrame(_env: *mut JNIEnv, _c: jclass, h: jlong) {
    let f = unsafe { &mut *(h as *mut front::Front) };
    f.count_frame();
}

/// Adds the second front buffer the layer flips to every refresh.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_dev_tabdisplay_Native_frontAddTwin(env: *mut JNIEnv, _c: jclass, h: jlong, hb: jobject) -> jni_sys::jboolean {
    let f = unsafe { &mut *(h as *mut front::Front) };
    unsafe { f.add_twin(env, hb) as jni_sys::jboolean }
}

/// Adds a swap-chain buffer; returns its index (-1 on failure).
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_dev_tabdisplay_Native_frontAddChainBuffer(env: *mut JNIEnv, _c: jclass, h: jlong, hb: jobject) -> jint {
    let f = unsafe { &mut *(h as *mut front::Front) };
    unsafe { f.add_chain_buffer(env, hb) }
}

/// Brings swap-chain buffer `i` up to date (it must not be on screen).
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_dev_tabdisplay_Native_frontRenderChain(_env: *mut JNIEnv, _c: jclass, h: jlong, i: jint) -> jni_sys::jboolean {
    let f = unsafe { &mut *(h as *mut front::Front) };
    f.render_chain(i.max(0) as usize) as jni_sys::jboolean
}

/// Debugging: how a YUV HardwareBuffer is laid out for CPU writes (logged).
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_dev_tabdisplay_Native_probeYuv(env: *mut JNIEnv, _c: jclass, hb: jobject) {
    unsafe {
        let b = front::AHardwareBuffer_fromHardwareBuffer(env, hb);
        let mut d = front::Desc::default();
        front::AHardwareBuffer_describe(b, &mut d);
        let mut planes: front::Planes = std::mem::zeroed();
        let st = front::AHardwareBuffer_lockPlanes(b, 2 << 4, -1, std::ptr::null(), &mut planes);
        let p = &planes.planes;
        front::log(&format!(
            "yuv probe: {}x{} format {:#x} usage {:#x} stride {} | lock {} planes {} | y {:p} px {} row {} | cb {:p} px {} row {} | cr {:p} px {} row {}",
            d.width, d.height, d.format, d.usage, d.stride, st, planes.count,
            p[0].data, p[0].pixel_stride, p[0].row_stride, p[1].data, p[1].pixel_stride, p[1].row_stride, p[2].data, p[2].pixel_stride, p[2].row_stride
        ));
        if st == 0 {
            front::AHardwareBuffer_unlock(b, std::ptr::null_mut());
        }
    }
}

/// An NV12 screen of `w` x `h` (landscape) pixels; returns its handle.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_dev_tabdisplay_Native_yuvCreate(_env: *mut JNIEnv, _c: jclass, w: jint, h: jint) -> jlong {
    Box::into_raw(Box::new(yuv::Screen::new(w.max(2) as usize & !1, h.max(2) as usize & !1))) as jlong
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_dev_tabdisplay_Native_yuvRelease(_env: *mut JNIEnv, _c: jclass, h: jlong) {
    if h != 0 {
        unsafe { drop(Box::from_raw(h as *mut yuv::Screen)) };
    }
}

/// Adds a swap-chain buffer (NV12/NV21 HardwareBuffer of the screen's size); returns its index.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_dev_tabdisplay_Native_yuvAddBuffer(env: *mut JNIEnv, _c: jclass, h: jlong, hb: jobject) -> jint {
    let s = unsafe { &mut *(h as *mut yuv::Screen) };
    unsafe { s.add_buffer(env, hb) }
}

/// Copies screen rectangle [x0, x1) x [y0, y1) from a YUV 4:2:0 picture in direct buffers
/// (pixel (0, 0) of which is screen pixel (ox, oy)).
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_dev_tabdisplay_Native_yuvUpdate(
    env: *mut JNIEnv, _c: jclass, h: jlong,
    y: jobject, y_stride: jint, u: jobject, v: jobject, uv_stride: jint, uv_step: jint,
    ox: jint, oy: jint, x0: jint, y0: jint, x1: jint, y1: jint,
) {
    let s = unsafe { &mut *(h as *mut yuv::Screen) };
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
        return;
    }
    let c = |v: jint| v.max(0) as usize;
    s.update(&src, front::Rect { x0: c(x0), y0: c(y0), x1: c(x1), y1: c(y1) });
}

/// Brings swap-chain buffer `i` (not on screen) up to date.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_dev_tabdisplay_Native_yuvRender(_env: *mut JNIEnv, _c: jclass, h: jlong, i: jint) -> jni_sys::jboolean {
    let s = unsafe { &mut *(h as *mut yuv::Screen) };
    s.render(i.max(0) as usize) as jni_sys::jboolean
}

/// The picture as RGBA into the direct buffer `out` (debugging).
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_dev_tabdisplay_Native_yuvDump(env: *mut JNIEnv, _c: jclass, h: jlong, out: jobject) {
    let s = unsafe { &*(h as *const yuv::Screen) };
    let p = direct(env, out) as *mut u32;
    let (w, hh) = s.size();
    if !p.is_null() {
        s.dump(unsafe { std::slice::from_raw_parts_mut(p, w * hh) });
    }
}

fn screen<'a>(h: jlong) -> &'a mut yuv::Screen {
    unsafe { &mut *(h as *mut yuv::Screen) }
}

/// Front mode: the NV12 buffer `hb` (screen-sized) is scanned out and written in place.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_dev_tabdisplay_Native_yuvAttachFront(env: *mut JNIEnv, _c: jclass, h: jlong, hb: jobject, hint: jint) -> jni_sys::jboolean {
    unsafe { screen(h).attach_front(env, hb, hint) as jni_sys::jboolean }
}

/// The panel's scan timing (front mode): a vsync (System.nanoTime clock) and the period, ns.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_dev_tabdisplay_Native_yuvVsync(_env: *mut JNIEnv, _c: jclass, h: jlong, vsync: jlong, period: jlong) {
    screen(h).set_vsync(vsync, period);
}

/// Front mode: writes everything changed since the last call, timed against the scan.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_dev_tabdisplay_Native_yuvPresent(_env: *mut JNIEnv, _c: jclass, h: jlong, frame: jni_sys::jboolean) -> jni_sys::jboolean {
    screen(h).present(frame != 0) as jni_sys::jboolean
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_dev_tabdisplay_Native_yuvCountFrame(_env: *mut JNIEnv, _c: jclass, h: jlong) {
    screen(h).count_frame();
}

/// The cursor picture (front mode): premultiplied RGBA in a direct buffer, w x h pixels.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_dev_tabdisplay_Native_yuvCursorImage(env: *mut JNIEnv, _c: jclass, h: jlong, rgba: jobject, w: jint, hgt: jint) {
    let p = direct(env, rgba);
    if p.is_null() || w <= 0 || hgt <= 0 {
        return;
    }
    let px = unsafe { std::slice::from_raw_parts(p, (w * hgt * 4) as usize) };
    screen(h).set_cursor_image(px, w as usize, hgt as usize);
}

/// Moves the cursor's top-left to (x, y) in screen pixels (front mode).
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_dev_tabdisplay_Native_yuvCursorMove(_env: *mut JNIEnv, _c: jclass, h: jlong, x: jint, y: jint, shown: jni_sys::jboolean) {
    screen(h).move_cursor(x as i64, y as i64, shown != 0);
}

// --- NV12 swap chain through ASurfaceControl (Android 10-12; chain.rs) -------------------------

fn chain<'a>(h: jlong) -> &'a std::sync::Arc<chain::Chain> {
    unsafe { &*(h as *const std::sync::Arc<chain::Chain>) }
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_dev_tabdisplay_Native_chainCreate(env: *mut JNIEnv, _c: jclass, surface: jobject, owner: jobject) -> jlong {
    match unsafe { chain::Chain::create(env, surface, owner) } {
        Some(c) => Box::into_raw(Box::new(c)) as jlong,
        None => 0,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_dev_tabdisplay_Native_chainAddBuffer(env: *mut JNIEnv, _c: jclass, h: jlong, hb: jobject) -> jni_sys::jboolean {
    unsafe { chain(h).add_buffer(env, hb) as jni_sys::jboolean }
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_dev_tabdisplay_Native_chainTake(_env: *mut JNIEnv, _c: jclass, h: jlong) -> jint {
    chain(h).take()
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_dev_tabdisplay_Native_chainSubmit(_env: *mut JNIEnv, _c: jclass, h: jlong, i: jint, batch: jlong) {
    chain(h).submit(i as usize, batch);
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_dev_tabdisplay_Native_chainRelease(env: *mut JNIEnv, _c: jclass, h: jlong) {
    let c = unsafe { Box::from_raw(h as *mut std::sync::Arc<chain::Chain>) };
    unsafe { c.release(env) };
}
