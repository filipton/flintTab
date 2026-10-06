package dev.tabdisplay

import android.hardware.HardwareBuffer
import android.view.Surface
import android.view.SurfaceView

/**
 * [YuvFront] for Android 10 to 12: the buffer is handed to the compositor through the NDK's
 * ASurfaceControl (android/native/src/chain.rs) and then written in place, as on 13+. Nothing
 * marks it a front buffer before 13, but the compositor puts an NV12 layer on a display plane of
 * its own, which reads the buffer from memory every refresh: writes are on the panel as the scan
 * passes, instead of 1.5 to 2.5 refreshes later through the compositor's queue.
 *
 * The panel's vsync comes from the present fences of the per-refresh hand-overs (when they
 * signaled), not the app's frame time, which is the app's wake-up (2 ms off on the MatePad).
 */
class NdkFront private constructor(
    override val handle: Long,
    private val buffer: HardwareBuffer,
) : FrontBuffer {
    private var chain = 0L
    private val notifier = android.os.HandlerThread("front-refresh").apply { start() }
    private val notifyHandler = android.os.Handler(notifier.looper)
    private val pending = java.util.concurrent.atomic.AtomicBoolean(false)

    override fun refresh() {
        if (!pending.compareAndSet(false, true)) return
        notifyHandler.post {
            pending.set(false)
            if (chain != 0L) Native.chainShow(chain, 0)
        }
    }

    override fun vsync(): Long = if (chain != 0L) Native.chainVsync(chain) else 0L

    override fun release() {
        notifier.quitSafely()
        notifier.join(500)
        if (chain != 0L) Native.chainRelease(chain)
        chain = 0L
        Native.yuvRelease(handle)
        buffer.close()
    }

    companion object {
        private const val USAGE = HardwareBuffer.USAGE_COMPOSER_OVERLAY or HardwareBuffer.USAGE_GPU_SAMPLED_IMAGE or
            HardwareBuffer.USAGE_CPU_WRITE_RARELY

        fun supported(): Boolean = android.os.Build.VERSION.SDK_INT in 29..32 && NdkChain.supported()

        /** A [bw] x [bh] buffer (the stream's size), scaled to [view]; null if not ready or failed. */
        fun attach(view: SurfaceView, bw: Int, bh: Int): NdkFront? {
            val surface = view.holder.surface
            if (surface == null || !surface.isValid || view.width == 0) return null
            val (w, h) = bw and 1.inv() to (bh and 1.inv())
            val buffer = try {
                HardwareBuffer.create(w, h, HardwareBuffer.YCBCR_420_888, 1, USAGE)
            } catch (e: Exception) {
                TLog.i("front buffer (NDK): cannot allocate: ${e.message}")
                return null
            }
            // Which way the panel scans the screen: the buffer transform hint (from 13 on) is the
            // display's rotation from the panel's own orientation.
            val hint = when (view.display?.rotation) {
                Surface.ROTATION_90 -> 4
                Surface.ROTATION_180 -> 3
                Surface.ROTATION_270 -> 7
                else -> 0
            }
            val handle = Native.yuvCreate(w, h)
            val f = NdkFront(handle, buffer)
            f.chain = Native.chainCreate(surface, f, w, h, view.width, view.height)
            // Written (all black) before it is shown.
            val ok = f.chain != 0L && Native.chainAddBuffer(f.chain, buffer) && Native.yuvAttachFront(handle, buffer, hint)
            if (!ok) {
                TLog.i("front buffer (NDK): the compositor layer could not be set up")
                f.release()
                return null
            }
            Native.chainShow(f.chain, 0)
            TLog.i("front buffer (NDK, Android ${android.os.Build.VERSION.SDK_INT}) ${w}x$h, shown at ${view.width}x${view.height}, scan hint $hint")
            return f
        }
    }
}
