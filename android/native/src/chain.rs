//! The NV12 swap chain through the NDK's ASurfaceControl (Android 10+), for systems older than
//! Android 13, where the Java API YuvChain.kt uses (SurfaceControl.Transaction.setBuffer) is
//! missing. The same thing: a few NV12 buffers (written by yuv.rs) handed to the compositor on a
//! layer under the app's SurfaceView, each written again only once the compositor gave it back.
//!
//! The compositor reports each transaction from one of its binder threads: when the buffer was
//! latched (shown), and the release fence of the buffer it replaced. Those go back to Kotlin
//! (NdkChain.presented / freed).

use std::{
    ffi::{CString, c_void},
    sync::{Arc, Mutex},
};

use jni_sys::{JNIEnv, JavaVM, jmethodID, jobject, jvalue};

use crate::front::{AHB, ARect, AHardwareBuffer_acquire, AHardwareBuffer_fromHardwareBuffer, AHardwareBuffer_release};

#[repr(C)]
pub struct ANativeWindow(c_void);
#[repr(C)]
pub struct ASurfaceControl(c_void);
#[repr(C)]
pub struct ASurfaceTransaction(c_void);
#[repr(C)]
pub struct ASurfaceTransactionStats(c_void);

type OnComplete = unsafe extern "C" fn(context: *mut c_void, stats: *mut ASurfaceTransactionStats);

#[link(name = "android")]
unsafe extern "C" {
    fn ANativeWindow_fromSurface(env: *mut JNIEnv, surface: jobject) -> *mut ANativeWindow;
    fn ANativeWindow_release(w: *mut ANativeWindow);
    fn ASurfaceControl_createFromWindow(parent: *mut ANativeWindow, name: *const libc::c_char) -> *mut ASurfaceControl;
    fn ASurfaceControl_release(sc: *mut ASurfaceControl);
    fn ASurfaceTransaction_create() -> *mut ASurfaceTransaction;
    fn ASurfaceTransaction_delete(t: *mut ASurfaceTransaction);
    fn ASurfaceTransaction_apply(t: *mut ASurfaceTransaction);
    fn ASurfaceTransaction_setBuffer(t: *mut ASurfaceTransaction, sc: *mut ASurfaceControl, b: *mut AHB, fence: i32);
    fn ASurfaceTransaction_setVisibility(t: *mut ASurfaceTransaction, sc: *mut ASurfaceControl, v: i8);
    fn ASurfaceTransaction_setZOrder(t: *mut ASurfaceTransaction, sc: *mut ASurfaceControl, z: i32);
    fn ASurfaceTransaction_setBufferTransparency(t: *mut ASurfaceTransaction, sc: *mut ASurfaceControl, tr: i8);
    fn ASurfaceTransaction_setBufferDataSpace(t: *mut ASurfaceTransaction, sc: *mut ASurfaceControl, ds: i32);
    fn ASurfaceTransaction_reparent(t: *mut ASurfaceTransaction, sc: *mut ASurfaceControl, parent: *mut ASurfaceControl);
    fn ASurfaceTransaction_setGeometry(
        t: *mut ASurfaceTransaction,
        sc: *mut ASurfaceControl,
        src: *const ARect,
        dst: *const ARect,
        transform: i32,
    );
    fn ASurfaceTransaction_setOnComplete(t: *mut ASurfaceTransaction, context: *mut c_void, f: OnComplete);
    fn ASurfaceTransactionStats_getLatchTime(stats: *mut ASurfaceTransactionStats) -> i64;
    fn ASurfaceTransactionStats_getPreviousReleaseFenceFd(stats: *mut ASurfaceTransactionStats, sc: *mut ASurfaceControl) -> i32;
}

const VISIBILITY_SHOW: i8 = 1;
const TRANSPARENCY_OPAQUE: i8 = 2;
/// BT.709, video range: what the Mac captures and the decoder outputs.
const DATASPACE_BT709: i32 = 281_083_904;

struct State {
    free: Vec<bool>,
    /// Per buffer: the compositor may still read it until this fence signals (-1: none).
    fence: Vec<i32>,
    /// The buffer on screen, and the one submitted last (shown once its transaction completes).
    shown: Option<usize>,
}

pub struct Chain {
    sc: *mut ASurfaceControl,
    bufs: Mutex<Vec<*mut AHB>>,
    state: Mutex<State>,
    vm: *mut JavaVM,
    /// The Kotlin NdkChain (a global reference) and its callbacks.
    owner: jobject,
    presented: jmethodID,
    freed: jmethodID,
}

unsafe impl Send for Chain {}
unsafe impl Sync for Chain {}

/// Per transaction: which buffer and which batch of frames it carries.
struct Pending {
    chain: Arc<Chain>,
    index: usize,
    batch: i64,
}

impl Chain {
    /// A layer on top of `surface` (the SurfaceView's), owned by the Kotlin object `owner`:
    /// buffers of `buf` (w, h) size, scaled by the compositor to `view` (w, h).
    pub unsafe fn create(env: *mut JNIEnv, surface: jobject, owner: jobject, buf: (i32, i32), view: (i32, i32)) -> Option<Arc<Chain>> {
        unsafe {
            let window = ANativeWindow_fromSurface(env, surface);
            if window.is_null() {
                return None;
            }
            let name = CString::new("tabdisplay-screen").unwrap();
            let sc = ASurfaceControl_createFromWindow(window, name.as_ptr());
            ANativeWindow_release(window);
            if sc.is_null() {
                return None;
            }
            let fns = **env;
            let mut vm: *mut JavaVM = std::ptr::null_mut();
            (fns.GetJavaVM.unwrap())(env, &mut vm);
            let class = (fns.GetObjectClass.unwrap())(env, owner);
            let method = |name: &str, sig: &str| {
                let (n, s) = (CString::new(name).unwrap(), CString::new(sig).unwrap());
                (fns.GetMethodID.unwrap())(env, class, n.as_ptr(), s.as_ptr())
            };
            let (presented, freed) = (method("presented", "(JJ)V"), method("freed", "()V"));
            if presented.is_null() || freed.is_null() {
                (fns.ExceptionClear.unwrap())(env);
                ASurfaceControl_release(sc);
                return None;
            }
            let owner = (fns.NewGlobalRef.unwrap())(env, owner);

            let t = ASurfaceTransaction_create();
            ASurfaceTransaction_setVisibility(t, sc, VISIBILITY_SHOW);
            ASurfaceTransaction_setZOrder(t, sc, 0);
            ASurfaceTransaction_setBufferTransparency(t, sc, TRANSPARENCY_OPAQUE);
            ASurfaceTransaction_setBufferDataSpace(t, sc, DATASPACE_BT709);
            let src = ARect { left: 0, top: 0, right: buf.0, bottom: buf.1 };
            let dst = ARect { left: 0, top: 0, right: view.0, bottom: view.1 };
            ASurfaceTransaction_setGeometry(t, sc, &src, &dst, 0);
            ASurfaceTransaction_apply(t);
            ASurfaceTransaction_delete(t);
            Some(Arc::new(Chain {
                sc,
                bufs: Mutex::new(Vec::new()),
                state: Mutex::new(State { free: Vec::new(), fence: Vec::new(), shown: None }),
                vm,
                owner,
                presented,
                freed,
            }))
        }
    }

    /// A buffer of the chain (the same ones yuv.rs writes, in the same order).
    pub unsafe fn add_buffer(&self, env: *mut JNIEnv, hb: jobject) -> bool {
        let b = unsafe { AHardwareBuffer_fromHardwareBuffer(env, hb) };
        if b.is_null() {
            return false;
        }
        unsafe { AHardwareBuffer_acquire(b) };
        self.bufs.lock().unwrap().push(b);
        let mut s = self.state.lock().unwrap();
        s.free.push(true);
        s.fence.push(-1);
        true
    }

    /// A buffer the compositor is done with, taken for writing (its fence waited for); -1 if
    /// every buffer is still with the compositor (Kotlin's `freed` runs once one comes back).
    pub fn take(&self) -> i32 {
        let fence = {
            let mut s = self.state.lock().unwrap();
            let Some(i) = s.free.iter().position(|&f| f) else { return -1 };
            s.free[i] = false;
            let fence = std::mem::replace(&mut s.fence[i], -1);
            (i, fence)
        };
        let (i, fd) = fence;
        if fd >= 0 {
            unsafe {
                let mut p = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
                libc::poll(&mut p, 1, 1000);
                libc::close(fd);
            }
        }
        i as i32
    }

    /// Shows buffer `i` (written by now); `batch` names its frames for `presented`.
    pub fn submit(self: &Arc<Self>, i: usize, batch: i64) {
        let Some(&b) = self.bufs.lock().unwrap().get(i) else { return };
        let pending = Box::into_raw(Box::new(Pending { chain: self.clone(), index: i, batch }));
        unsafe {
            let t = ASurfaceTransaction_create();
            ASurfaceTransaction_setBuffer(t, self.sc, b, -1);
            ASurfaceTransaction_setOnComplete(t, pending.cast(), on_complete);
            ASurfaceTransaction_apply(t);
            ASurfaceTransaction_delete(t);
        }
    }

    unsafe fn call(&self, method: jmethodID, args: &[jvalue]) {
        unsafe {
            let vm = self.vm;
            let mut env: *mut JNIEnv = std::ptr::null_mut();
            let attach = (**vm).AttachCurrentThreadAsDaemon.unwrap();
            if attach(vm, (&mut env as *mut *mut JNIEnv).cast(), std::ptr::null_mut()) != 0 || env.is_null() {
                return;
            }
            ((**env).CallVoidMethodA.unwrap())(env, self.owner, method, args.as_ptr());
            if ((**env).ExceptionCheck.unwrap())(env) != 0 {
                ((**env).ExceptionClear.unwrap())(env);
            }
        }
    }

    /// Off screen; the buffers and the layer are given back.
    pub unsafe fn release(&self, env: *mut JNIEnv) {
        unsafe {
            let t = ASurfaceTransaction_create();
            ASurfaceTransaction_reparent(t, self.sc, std::ptr::null_mut());
            ASurfaceTransaction_apply(t);
            ASurfaceTransaction_delete(t);
            ((**env).DeleteGlobalRef.unwrap())(env, self.owner);
        }
    }
}

impl Drop for Chain {
    fn drop(&mut self) {
        unsafe {
            for &b in self.bufs.lock().unwrap().iter() {
                AHardwareBuffer_release(b);
            }
            for &fd in &self.state.lock().unwrap().fence {
                if fd >= 0 {
                    libc::close(fd);
                }
            }
            ASurfaceControl_release(self.sc);
        }
    }
}

/// A transaction reached the screen: the buffer it showed is now on screen, the one before it
/// is free once its release fence signals.
unsafe extern "C" fn on_complete(context: *mut c_void, stats: *mut ASurfaceTransactionStats) {
    let p = unsafe { Box::from_raw(context.cast::<Pending>()) };
    let chain = &p.chain;
    let latch = unsafe { ASurfaceTransactionStats_getLatchTime(stats) };
    let release = unsafe { ASurfaceTransactionStats_getPreviousReleaseFenceFd(stats, chain.sc) };
    {
        let mut s = chain.state.lock().unwrap();
        if let Some(prev) = s.shown.replace(p.index).filter(|&prev| prev != p.index) {
            let old = std::mem::replace(&mut s.fence[prev], release);
            if old >= 0 {
                unsafe { libc::close(old) };
            }
            s.free[prev] = true;
        } else if release >= 0 {
            unsafe { libc::close(release) };
        }
    }
    unsafe {
        chain.call(chain.presented, &[jvalue { j: p.batch }, jvalue { j: latch }]);
        chain.call(chain.freed, &[]);
    }
}
