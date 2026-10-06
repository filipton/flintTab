//! The H.264 decoder through the NDK's AMediaCodec, for the CPU renderer: compressed frames go
//! from the Java byte array straight into the codec's input buffer, and decoded frames come
//! back on the codec's own thread as raw buffers (Kotlin gets direct ByteBuffers over them).
//!
//! The Java MediaCodec does the same work but hands every callback to a Java Handler thread
//! (a wake-up per frame, ~1 ms on this hardware) and wraps each output in an Image.

use std::{
    collections::VecDeque,
    ffi::{CString, c_char, c_void},
    sync::{Arc, Condvar, Mutex},
    time::{Duration, Instant},
};

use jni_sys::{JNIEnv, JavaVM, jbyteArray, jmethodID, jobject, jvalue};

#[repr(C)]
pub struct AMediaCodec(c_void);
#[repr(C)]
pub struct AMediaFormat(c_void);

#[repr(C)]
struct BufferInfo {
    offset: i32,
    size: i32,
    pts_us: i64,
    flags: u32,
}

#[repr(C)]
struct AsyncCallbacks {
    input: unsafe extern "C" fn(*mut AMediaCodec, *mut c_void, i32),
    output: unsafe extern "C" fn(*mut AMediaCodec, *mut c_void, i32, *mut BufferInfo),
    format: unsafe extern "C" fn(*mut AMediaCodec, *mut c_void, *mut AMediaFormat),
    error: unsafe extern "C" fn(*mut AMediaCodec, *mut c_void, i32, i32, *const c_char),
}

#[link(name = "mediandk")]
unsafe extern "C" {
    fn AMediaCodec_createCodecByName(name: *const c_char) -> *mut AMediaCodec;
    fn AMediaCodec_delete(c: *mut AMediaCodec) -> i32;
    fn AMediaCodec_configure(c: *mut AMediaCodec, f: *const AMediaFormat, surface: *mut c_void, crypto: *mut c_void, flags: u32) -> i32;
    fn AMediaCodec_setAsyncNotifyCallback(c: *mut AMediaCodec, cb: AsyncCallbacks, userdata: *mut c_void) -> i32;
    fn AMediaCodec_start(c: *mut AMediaCodec) -> i32;
    fn AMediaCodec_stop(c: *mut AMediaCodec) -> i32;
    fn AMediaCodec_getInputBuffer(c: *mut AMediaCodec, idx: usize, size: *mut usize) -> *mut u8;
    fn AMediaCodec_queueInputBuffer(c: *mut AMediaCodec, idx: usize, offset: i64, size: usize, pts: u64, flags: u32) -> i32;
    fn AMediaCodec_getOutputBuffer(c: *mut AMediaCodec, idx: usize, size: *mut usize) -> *mut u8;
    fn AMediaCodec_releaseOutputBuffer(c: *mut AMediaCodec, idx: usize, render: bool) -> i32;
    fn AMediaCodec_getOutputFormat(c: *mut AMediaCodec) -> *mut AMediaFormat;
    fn AMediaFormat_new() -> *mut AMediaFormat;
    fn AMediaFormat_delete(f: *mut AMediaFormat) -> i32;
    fn AMediaFormat_setString(f: *mut AMediaFormat, name: *const c_char, value: *const c_char);
    fn AMediaFormat_setInt32(f: *mut AMediaFormat, name: *const c_char, value: i32);
    fn AMediaFormat_getInt32(f: *mut AMediaFormat, name: *const c_char, out: *mut i32) -> bool;
}

/// How the decoded pictures are laid out (from the codec's output format).
#[derive(Clone, Copy, Default)]
struct Layout {
    width: i32,
    height: i32,
    stride: i32,
    slice_height: i32,
    /// 19: planar (Y, U, V); anything else: semi-planar (Y, then Cb Cr pairs).
    color: i32,
    crop: [i32; 4],
}

struct Inner {
    free: VecDeque<usize>,
    layout: Layout,
    error: Option<String>,
}

pub struct Codec {
    codec: *mut AMediaCodec,
    inner: Mutex<Inner>,
    input_ready: Condvar,
    vm: *mut JavaVM,
    /// The Kotlin Decoder (a global reference) and its callbacks.
    owner: jobject,
    on_frame: jmethodID,
    on_error: jmethodID,
    /// False once released: buffers handed back late (the renderer may hold one) are ignored.
    alive: Mutex<bool>,
    /// Buffers to give back, for the thread that does it: giving one back waits on the codec's
    /// own thread, which is the one a picture is drawn from (inside its callback).
    returns: Mutex<Option<std::sync::mpsc::Sender<usize>>>,
}

unsafe impl Send for Codec {}
unsafe impl Sync for Codec {}

fn cstr(s: &str) -> CString {
    CString::new(s).unwrap()
}

fn get_i32(f: *mut AMediaFormat, key: &str) -> Option<i32> {
    let mut v = 0;
    unsafe { AMediaFormat_getInt32(f, cstr(key).as_ptr(), &mut v) }.then_some(v)
}

impl Codec {
    /// `keys`/`values`: integer format entries (low-latency options, max size).
    pub unsafe fn create(env: *mut JNIEnv, owner: jobject, name: &str, mime: &str, w: i32, h: i32, opts: &[(String, i32)]) -> Result<Arc<Codec>, String> {
        unsafe {
            let codec = AMediaCodec_createCodecByName(cstr(name).as_ptr());
            if codec.is_null() {
                return Err(format!("no codec named {name}"));
            }
            let fns = **env;
            let mut vm: *mut JavaVM = std::ptr::null_mut();
            (fns.GetJavaVM.unwrap())(env, &mut vm);
            let class = (fns.GetObjectClass.unwrap())(env, owner);
            let method = |n: &str, sig: &str| (fns.GetMethodID.unwrap())(env, class, cstr(n).as_ptr(), cstr(sig).as_ptr());
            let on_frame = method("onNativeFrame", "(IJLjava/nio/ByteBuffer;Ljava/nio/ByteBuffer;Ljava/nio/ByteBuffer;IIIIIII)V");
            let on_error = method("onNativeError", "(Ljava/lang/String;)V");
            if on_frame.is_null() || on_error.is_null() {
                (fns.ExceptionClear.unwrap())(env);
                AMediaCodec_delete(codec);
                return Err("the Kotlin decoder lacks its callbacks".into());
            }
            let owner = (fns.NewGlobalRef.unwrap())(env, owner);
            let c = Arc::new(Codec {
                codec,
                inner: Mutex::new(Inner { free: VecDeque::new(), layout: Layout::default(), error: None }),
                input_ready: Condvar::new(),
                vm,
                owner,
                on_frame,
                on_error,
                alive: Mutex::new(true),
                returns: Mutex::new(None),
            });
            let (tx, rx) = std::sync::mpsc::channel::<usize>();
            *c.returns.lock().unwrap() = Some(tx);
            let weak = Arc::downgrade(&c);
            std::thread::Builder::new()
                .name("codec-return".into())
                .spawn(move || {
                    for idx in rx {
                        let Some(c) = weak.upgrade() else { break };
                        let alive = c.alive.lock().unwrap();
                        if *alive {
                            AMediaCodec_releaseOutputBuffer(c.codec, idx, false);
                        }
                    }
                })
                .ok();
            let fmt = AMediaFormat_new();
            AMediaFormat_setString(fmt, cstr("mime").as_ptr(), cstr(mime).as_ptr());
            AMediaFormat_setInt32(fmt, cstr("width").as_ptr(), w);
            AMediaFormat_setInt32(fmt, cstr("height").as_ptr(), h);
            for (k, v) in opts {
                AMediaFormat_setInt32(fmt, cstr(k).as_ptr(), *v);
            }
            // The callbacks hold a reference of their own, given back in release().
            let user = Arc::into_raw(c.clone()) as *mut c_void;
            let cbs = AsyncCallbacks { input: on_input, output: on_output, format: on_format, error: on_error_cb };
            let ok = AMediaCodec_setAsyncNotifyCallback(codec, cbs, user) == 0
                && AMediaCodec_configure(codec, fmt, std::ptr::null_mut(), std::ptr::null_mut(), 0) == 0
                && AMediaCodec_start(codec) == 0;
            AMediaFormat_delete(fmt);
            if !ok {
                c.release(env);
                drop(Arc::from_raw(user as *const Codec));
                return Err("configure/start refused".into());
            }
            c.read_layout(AMediaCodec_getOutputFormat(codec), true);
            Ok(c)
        }
    }

    /// `owned`: the format is ours to delete (else the caller's, e.g. a callback's argument).
    fn read_layout(&self, f: *mut AMediaFormat, owned: bool) {
        if f.is_null() {
            return;
        }
        let mut l = self.inner.lock().unwrap().layout;
        if let Some(v) = get_i32(f, "width") {
            l.width = v;
        }
        if let Some(v) = get_i32(f, "height") {
            l.height = v;
        }
        l.stride = get_i32(f, "stride").unwrap_or(l.width);
        l.slice_height = get_i32(f, "slice-height").unwrap_or(l.height);
        l.color = get_i32(f, "color-format").unwrap_or(l.color);
        l.crop = [
            get_i32(f, "crop-left").unwrap_or(0),
            get_i32(f, "crop-top").unwrap_or(0),
            get_i32(f, "crop-right").unwrap_or(l.width - 1),
            get_i32(f, "crop-bottom").unwrap_or(l.height - 1),
        ];
        if l.stride <= 0 {
            l.stride = l.width;
        }
        if l.slice_height <= 0 {
            l.slice_height = l.height;
        }
        self.inner.lock().unwrap().layout = l;
        if owned {
            unsafe { AMediaFormat_delete(f) };
        }
    }

    /// Hands `au` to the codec (waits up to `timeout` for an input buffer). Returns the time
    /// spent queueing (ns), or an error.
    pub unsafe fn feed(&self, env: *mut JNIEnv, au: jbyteArray, size: usize, pts: i64, timeout: Duration) -> Result<i64, String> {
        let deadline = Instant::now() + timeout;
        let idx = {
            let mut g = self.inner.lock().unwrap();
            loop {
                if let Some(e) = &g.error {
                    return Err(e.clone());
                }
                if let Some(i) = g.free.pop_front() {
                    break i;
                }
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Err(format!("took no input for {} ms", timeout.as_millis()));
                }
                g = self.input_ready.wait_timeout(g, left).unwrap().0;
            }
        };
        unsafe {
            let mut cap = 0usize;
            let buf = AMediaCodec_getInputBuffer(self.codec, idx, &mut cap);
            if buf.is_null() || cap < size {
                return Err(format!("input buffer {cap} bytes for a {size}-byte frame"));
            }
            ((**env).GetByteArrayRegion.unwrap())(env, au, 0, size as i32, buf.cast());
            let t = Instant::now();
            if AMediaCodec_queueInputBuffer(self.codec, idx, 0, size, pts as u64, 0) != 0 {
                return Err("queueInputBuffer refused".into());
            }
            Ok(t.elapsed().as_nanos() as i64)
        }
    }

    /// Gives decoded buffer `idx` back (its picture was copied out).
    pub fn done(&self, idx: usize) {
        if let Some(tx) = self.returns.lock().unwrap().as_ref() {
            let _ = tx.send(idx);
        }
    }

    /// Stops and deletes the codec (no callback runs after this).
    pub unsafe fn release(&self, env: *mut JNIEnv) {
        {
            let mut alive = self.alive.lock().unwrap();
            if !*alive {
                return;
            }
            *alive = false;
            self.returns.lock().unwrap().take(); // ends the return thread
            self.inner.lock().unwrap().error = Some("released".into());
            unsafe {
                AMediaCodec_stop(self.codec);
                AMediaCodec_delete(self.codec);
            }
        }
        unsafe { ((**env).DeleteGlobalRef.unwrap())(env, self.owner) };
        self.input_ready.notify_all();
    }

    unsafe fn env(&self) -> Option<*mut JNIEnv> {
        unsafe {
            let mut env: *mut JNIEnv = std::ptr::null_mut();
            let attach = (**self.vm).AttachCurrentThreadAsDaemon.unwrap();
            (attach(self.vm, (&mut env as *mut *mut JNIEnv).cast(), std::ptr::null_mut()) == 0 && !env.is_null()).then_some(env)
        }
    }
}

unsafe fn codec_of<'a>(user: *mut c_void) -> &'a Codec {
    unsafe { &*(user as *const Codec) }
}

unsafe extern "C" fn on_input(_c: *mut AMediaCodec, user: *mut c_void, idx: i32) {
    let c = unsafe { codec_of(user) };
    c.inner.lock().unwrap().free.push_back(idx as usize);
    c.input_ready.notify_one();
}

unsafe extern "C" fn on_format(_c: *mut AMediaCodec, user: *mut c_void, f: *mut AMediaFormat) {
    let c = unsafe { codec_of(user) };
    // Read from the callback's own format (the NDK frees it): asking the codec for its format
    // from its own callback thread waits on that very thread, for ever.
    c.read_layout(f, false);
}

unsafe extern "C" fn on_error_cb(_c: *mut AMediaCodec, user: *mut c_void, err: i32, action: i32, detail: *const c_char) {
    let c = unsafe { codec_of(user) };
    let detail = if detail.is_null() { String::new() } else { unsafe { std::ffi::CStr::from_ptr(detail) }.to_string_lossy().into_owned() };
    let msg = format!("codec error {err} (action {action}) {detail}");
    c.inner.lock().unwrap().error = Some(msg.clone());
    c.input_ready.notify_all();
    unsafe {
        if let Some(env) = c.env() {
            let s = ((**env).NewStringUTF.unwrap())(env, cstr(&msg).as_ptr());
            ((**env).CallVoidMethodA.unwrap())(env, c.owner, c.on_error, [jvalue { l: s }].as_ptr());
            ((**env).ExceptionClear.unwrap())(env);
            ((**env).DeleteLocalRef.unwrap())(env, s);
        }
    }
}

/// A decoded picture: straight to Kotlin as direct buffers over the codec's own memory, on
/// this (the codec's) thread; Kotlin copies it out and calls done().
unsafe extern "C" fn on_output(_c: *mut AMediaCodec, user: *mut c_void, idx: i32, info: *mut BufferInfo) {
    let c = unsafe { codec_of(user) };
    let info = unsafe { &*info };
    let l = c.inner.lock().unwrap().layout;
    unsafe {
        let mut size = 0usize;
        let base = AMediaCodec_getOutputBuffer(c.codec, idx as usize, &mut size);
        static LOGGED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        if LOGGED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 2 {
            crate::front::log(&format!(
                "NDK decoder output: buffer {} ({size} bytes, info offset {} size {} flags {}), {}x{} stride {} slice {} color {:#x} crop {:?}",
                if base.is_null() { "null" } else { "ok" }, info.offset, info.size, info.flags, l.width, l.height, l.stride, l.slice_height, l.color, l.crop
            ));
        }
        let Some(env) = c.env() else {
            c.done(idx as usize);
            return;
        };
        // Only layouts known here: NV12 (21) and I420 (19). A vendor layout (tiled, compressed)
        // read as one would be garbage: the Java decoder converts those itself.
        if l.color != 19 && l.color != 21 {
            c.done(idx as usize);
            let msg = format!("unsupported output layout {:#x}", l.color);
            c.inner.lock().unwrap().error = Some(msg.clone());
            let s = ((**env).NewStringUTF.unwrap())(env, cstr(&msg).as_ptr());
            ((**env).CallVoidMethodA.unwrap())(env, c.owner, c.on_error, [jvalue { l: s }].as_ptr());
            ((**env).ExceptionClear.unwrap())(env);
            ((**env).DeleteLocalRef.unwrap())(env, s);
            return;
        }
        if base.is_null() || info.size <= 0 || l.stride <= 0 {
            c.done(idx as usize);
            return;
        }
        let base = base.add(info.offset.max(0) as usize);
        let size = (info.size as usize).min(size);
        let (stride, sh) = (l.stride as usize, l.slice_height as usize);
        let y_len = stride * sh;
        // The last chroma row may stop at the picture's width (no padding after it).
        if y_len + stride * (sh / 2 - 1) + (l.width.max(0) as usize) > size {
            c.done(idx as usize);
            return;
        }
        let nb = (**env).NewDirectByteBuffer.unwrap();
        // Semi-planar: Cb Cr pairs after Y (step 2). Planar: U then V planes (step 1).
        let (u, v, uv_stride, step) = if l.color == 19 {
            let q = (stride / 2) * (sh / 2);
            (nb(env, base.add(y_len).cast(), q as i64), nb(env, base.add(y_len + q).cast(), q as i64), stride / 2, 1)
        } else {
            let n = stride * (sh / 2);
            (nb(env, base.add(y_len).cast(), n as i64), nb(env, base.add(y_len + 1).cast(), (n - 1) as i64), stride, 2)
        };
        let y = nb(env, base.cast(), y_len as i64);
        let args = [
            jvalue { i: idx },
            jvalue { j: info.pts_us },
            jvalue { l: y },
            jvalue { l: u },
            jvalue { l: v },
            jvalue { i: stride as i32 },
            jvalue { i: uv_stride as i32 },
            jvalue { i: step },
            jvalue { i: l.crop[0] },
            jvalue { i: l.crop[1] },
            jvalue { i: l.crop[2] - l.crop[0] + 1 },
            jvalue { i: l.crop[3] - l.crop[1] + 1 },
        ];
        ((**env).CallVoidMethodA.unwrap())(env, c.owner, c.on_frame, args.as_ptr());
        if ((**env).ExceptionCheck.unwrap())(env) != 0 {
            ((**env).ExceptionDescribe.unwrap())(env);
            ((**env).ExceptionClear.unwrap())(env);
        }
        for r in [y, u, v] {
            ((**env).DeleteLocalRef.unwrap())(env, r);
        }
    }
}

/// The callbacks' own reference, given back once the codec is stopped and deleted.
pub unsafe fn forget_callbacks(c: &Arc<Codec>) {
    unsafe { drop(Arc::from_raw(Arc::as_ptr(c))) };
}
