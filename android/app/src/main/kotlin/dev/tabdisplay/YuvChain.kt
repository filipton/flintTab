package dev.tabdisplay

import android.hardware.DataSpace
import android.hardware.HardwareBuffer
import android.hardware.SyncFence
import android.os.Handler
import android.view.SurfaceControl
import android.view.SurfaceView

/**
 * The screen as a swap chain of NV12 buffers the compositor shows (the default renderer).
 *
 * YUV, not RGBA: a full-screen RGBA layer made the display controller read 13 MB per frame,
 * and at 90 Hz it fell behind and left gray in the upper half of the panel (the system's own
 * video path hands it compressed YUV). NV12 is 1.5 bytes per pixel, what the tablet receives
 * anyway, and the display rotates and converts it itself.
 *
 * Each buffer is written only while not on screen (no tearing); the native side
 * (android/native/src/yuv.rs) keeps the picture and copies each buffer only what it is missing.
 */
class YuvChain private constructor(
    val handle: Long,
    private val sc: SurfaceControl,
    private val buffers: List<HardwareBuffer>,
    private val handler: Handler,
) {
    private val free = BooleanArray(buffers.size) { true }
    private val releaseFence = arrayOfNulls<SyncFence>(buffers.size)
    private var pendingRetry: (() -> Unit)? = null
    /** Waits for present fences, off the render thread. */
    private val fences = java.util.concurrent.Executors.newSingleThreadExecutor()

    /**
     * Brings a free buffer up to date and hands it to the compositor; [onShown] gets the time
     * it reached the screen (its present fence). False if every buffer is still with the
     * compositor: [retry] runs once one comes back. Render thread only.
     */
    fun submit(onShown: (Long) -> Unit, retry: () -> Unit): Boolean {
        val i = free.indexOfFirst { it }
        if (i < 0) {
            pendingRetry = retry
            return false
        }
        free[i] = false
        releaseFence[i]?.let {
            it.awaitForever() // the compositor may still be reading it
            it.close()
            releaseFence[i] = null
        }
        Native.yuvRender(handle, i)
        val t = SurfaceControl.Transaction()
        t.setBuffer(sc, buffers[i], null) { fence ->
            handler.post {
                releaseFence[i] = fence
                free[i] = true
                pendingRetry?.let {
                    pendingRetry = null
                    it()
                }
            }
        }
        t.addTransactionCompletedListener({ it.run() }) { stats ->
            val fence = stats.presentFence
            fences.execute {
                fence.awaitForever()
                val at = fence.signalTime
                fence.close()
                if (at > 0) onShown(at)
            }
        }
        t.apply()
        return true
    }

    fun release() {
        fences.shutdownNow()
        SurfaceControl.Transaction().reparent(sc, null).apply()
        sc.release()
        Native.yuvRelease(handle)
        buffers.forEach { it.close() }
    }

    companion object {
        /** NV12 (with GPU usage the allocator picks Cb-first) the display can scan out. */
        private const val USAGE = HardwareBuffer.USAGE_COMPOSER_OVERLAY or HardwareBuffer.USAGE_GPU_SAMPLED_IMAGE or
            HardwareBuffer.USAGE_CPU_WRITE_RARELY

        fun supported(): Boolean = android.os.Build.VERSION.SDK_INT >= 33 && try {
            HardwareBuffer.isSupported(2304, 1440, HardwareBuffer.YCBCR_420_888, 1, USAGE)
        } catch (_: Throwable) {
            false
        }

        /** Four buffers the size of [view] (landscape, as received); null if not ready. */
        fun attach(view: SurfaceView, handler: Handler): YuvChain? {
            val parent = view.surfaceControl
            if (!parent.isValid || view.width == 0) return null
            val (w, h) = view.width and 1.inv() to (view.height and 1.inv())
            val buffers = try {
                // The compositor holds two (on screen, queued): with four, one is always free.
                List(4) { HardwareBuffer.create(w, h, HardwareBuffer.YCBCR_420_888, 1, USAGE) }
            } catch (_: Exception) {
                return null
            }
            val handle = Native.yuvCreate(w, h)
            if (buffers.any { Native.yuvAddBuffer(handle, it) < 0 }) {
                Native.yuvRelease(handle)
                buffers.forEach { it.close() }
                return null
            }
            val sc = SurfaceControl.Builder().setName("tabdisplay-screen").setParent(parent).build()
            SurfaceControl.Transaction()
                // What the Mac captures and the decoder outputs: BT.709, video range.
                .setDataSpace(sc, DataSpace.pack(DataSpace.STANDARD_BT709, DataSpace.TRANSFER_SMPTE_170M, DataSpace.RANGE_LIMITED))
                .setOpaque(sc, true)
                .setLayer(sc, 0)
                .setVisibility(sc, true)
                .apply()
            android.util.Log.i("tabdisplay", "yuv chain: 4 NV12 buffers ${w}x$h")
            return YuvChain(handle, sc, buffers, handler)
        }
    }
}
