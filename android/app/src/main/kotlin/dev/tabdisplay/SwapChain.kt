package dev.tabdisplay

import android.hardware.HardwareBuffer
import android.hardware.SyncFence
import android.os.Handler
import android.view.SurfaceControl
import android.view.SurfaceView

/**
 * The screen as a regular swap chain of CPU-written buffers handed to the compositor.
 *
 * Each buffer is written only while the display is not showing it (the compositor says when
 * it gives one back), so nothing tears and the display controller sees an ordinary new frame
 * every time. Writing into a buffer the panel was showing (front-buffer rendering) left areas
 * of this tablet's LCD unrefreshed: they faded to gray.
 *
 * The native side (android/native/src/front.rs) keeps the picture in memory and, per buffer,
 * the areas it is missing; bringing a buffer up to date only copies those.
 */
class SwapChain private constructor(
    val handle: Long,
    private val sc: SurfaceControl,
    private val buffers: List<HardwareBuffer>,
    private val handler: Handler,
) {
    /** Per buffer: free to write, and the fence to wait for before writing it. */
    private val free = BooleanArray(buffers.size) { true }
    private val releaseFence = arrayOfNulls<SyncFence>(buffers.size)
    private var waiting = false

    /**
     * Brings a free buffer up to date and hands it to the compositor; [onShown] gets the time
     * it reached the screen (its present fence). Returns false if every buffer is still with
     * the compositor: [retry] runs once one comes back. Render thread only.
     */
    fun submit(onShown: (Long) -> Unit, retry: () -> Unit): Boolean {
        val i = free.indexOfFirst { it }
        if (i < 0) {
            waiting = true
            pendingRetry = retry
            return false
        }
        free[i] = false
        releaseFence[i]?.let {
            it.awaitForever() // the compositor may still be reading it
            it.close()
            releaseFence[i] = null
        }
        Native.frontRenderChain(handle, i)
        val t = SurfaceControl.Transaction()
        t.setBuffer(sc, buffers[i], null) { fence ->
            handler.post {
                releaseFence[i] = fence
                free[i] = true
                if (waiting) {
                    waiting = false
                    pendingRetry?.invoke()
                }
            }
        }
        t.addTransactionCompletedListener({ it.run() }) { stats ->
            // When this frame went on screen (or would have, if a newer one replaced it).
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

    private var pendingRetry: (() -> Unit)? = null
    /** Waits for present fences, off the render thread. */
    private val fences = java.util.concurrent.Executors.newSingleThreadExecutor()

    fun release() {
        fences.shutdownNow()
        SurfaceControl.Transaction().reparent(sc, null).apply()
        sc.release()
        Native.frontRelease(handle)
        buffers.forEach { it.close() }
    }

    companion object {
        /**
         * Written by the CPU while not on screen. GPU-readable too: the compositor falls back to
         * GPU composition when it likes (and screenshots always use it), and without this usage
         * it cannot read the buffer.
         */
        private const val USAGE = HardwareBuffer.USAGE_COMPOSER_OVERLAY or HardwareBuffer.USAGE_GPU_SAMPLED_IMAGE or
            HardwareBuffer.USAGE_CPU_WRITE_RARELY or HardwareBuffer.USAGE_CPU_READ_RARELY

        fun supported(): Boolean = android.os.Build.VERSION.SDK_INT >= 33 && try {
            HardwareBuffer.isSupported(2304, 1440, HardwareBuffer.RGBA_8888, 1, USAGE)
        } catch (_: Throwable) {
            false
        }

        /** Four buffers over [view]; null if the view is not ready or the device cannot. */
        fun attach(view: SurfaceView, handler: Handler): SwapChain? {
            val parent = view.surfaceControl
            if (!parent.isValid || view.width == 0) return null
            // In the panel's own orientation and rotated back by the layer, so the display
            // hardware scans them out as they are.
            val hint = view.rootSurfaceControl?.bufferTransformHint ?: 0
            val swap = hint == SurfaceControl.BUFFER_TRANSFORM_ROTATE_90 || hint == SurfaceControl.BUFFER_TRANSFORM_ROTATE_270
            val (bw, bh) = if (swap) view.height to view.width else view.width to view.height
            val buffers = try {
                // The compositor holds two (on screen, queued): with four, one is always free.
                List(4) { HardwareBuffer.create(bw, bh, HardwareBuffer.RGBA_8888, 1, USAGE) }
            } catch (_: Exception) {
                return null
            }
            val handle = Native.frontAttach(buffers[0], hint)
            if (handle == 0L || buffers.any { Native.frontAddChainBuffer(handle, it) < 0 }) {
                if (handle != 0L) Native.frontRelease(handle)
                buffers.forEach { it.close() }
                return null
            }
            val inverse = when (hint) {
                SurfaceControl.BUFFER_TRANSFORM_ROTATE_90 -> SurfaceControl.BUFFER_TRANSFORM_ROTATE_270
                SurfaceControl.BUFFER_TRANSFORM_ROTATE_270 -> SurfaceControl.BUFFER_TRANSFORM_ROTATE_90
                else -> hint
            }
            val sc = SurfaceControl.Builder().setName("tabdisplay-screen").setParent(parent).build()
            SurfaceControl.Transaction()
                .setBufferTransform(sc, inverse)
                .setLayer(sc, 1)
                .setVisibility(sc, true)
                .apply()
            android.util.Log.i("tabdisplay", "swap chain: 4 buffers ${bw}x$bh, hint $hint")
            return SwapChain(handle, sc, buffers, handler)
        }
    }
}
