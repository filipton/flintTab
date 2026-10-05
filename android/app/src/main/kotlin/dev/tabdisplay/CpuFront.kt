package dev.tabdisplay

import android.hardware.HardwareBuffer
import android.view.SurfaceControl
import android.view.SurfaceView

/**
 * A front buffer written by the CPU (Rust, android/native/src/front.rs): one RGBA buffer,
 * attached once to a layer the display hardware scans out directly, then updated in place.
 * Every write is on the panel as soon as its scan line comes by: no GPU, no compositor.
 */
class CpuFront private constructor(
    val handle: Long,
    private val sc: SurfaceControl,
    private val buffer: HardwareBuffer,
    /** The identical second buffer flipped to every refresh; null for the single-buffer variant. */
    private val twin: HardwareBuffer?,
) {
    private var showingTwin = false

    /**
     * Shows the other of the two (identical) buffers, as a new frame with full damage. The
     * display controller here refreshes only what new buffers' damage covers: with one buffer
     * that never "changes", areas of the LCD stopped being refreshed and faded to gray. Both
     * buffers always hold the latest pixels, so flipping never delays an update.
     */
    fun flip() {
        val next = (if (showingTwin) buffer else twin) ?: return
        showingTwin = !showingTwin
        if (!notifyPending.compareAndSet(false, true)) return
        notifyHandler.post {
            notifyPending.set(false)
            try {
                SurfaceControl.Transaction().setBuffer(sc, next).setDamageRegion(sc, fullDamage).apply()
            } catch (_: Throwable) {}
        }
    }

    private val notifier = android.os.HandlerThread("front-damage").apply { start() }
    private val notifyHandler = android.os.Handler(notifier.looper)
    private val notifyPending = java.util.concurrent.atomic.AtomicBoolean(false)

    /**
     * Tells the compositor the layer changed (the same buffer again). The pixels are already
     * on their way to the panel; this keeps the display out of its idle mode, in which the
     * display controller stops refreshing the panel every frame (and drops to 30 Hz): CPU
     * writes alone are invisible to the compositor, and the panel then showed stale stripes.
     * Off the render thread and at most one at a time.
     */
    fun damaged() {
        if (noDamage) return
        if (!notifyPending.compareAndSet(false, true)) return
        notifyHandler.post {
            notifyPending.set(false)
            try {
                // The whole buffer as damage: the display controller then sends all of it to
                // the panel (some skip "static" layers or send only damaged parts).
                SurfaceControl.Transaction().setBuffer(sc, buffer).setDamageRegion(sc, fullDamage).apply()
            } catch (_: Throwable) {}
        }
    }

    private val fullDamage = android.graphics.Region(0, 0, buffer.width, buffer.height)

    fun release() {
        notifier.quitSafely()
        SurfaceControl.Transaction().reparent(sc, null).apply()
        sc.release()
        Native.frontRelease(handle)
        buffer.close()
        twin?.close()
    }

    companion object {
        // CPU write *rarely*: an uncached (write-combined) mapping, so every write goes straight
        // to the memory the display controller reads. "Often" gives a cached mapping, and cache
        // lines not yet written back showed on the panel as gray stripes of older pictures.
        // Read only by the debugging dump.
        const val USAGE = HardwareBuffer.USAGE_FRONT_BUFFER or HardwareBuffer.USAGE_COMPOSER_OVERLAY or
            HardwareBuffer.USAGE_CPU_WRITE_RARELY or HardwareBuffer.USAGE_CPU_READ_RARELY

        /** Experiment (variant B): also allocated as a GPU buffer, like the GL path's. */
        @Volatile var gpuUsage = false
        /** Experiment (variant A = 1): one buffer, re-submitted after writes. */
        @Volatile var singleBuffer = false
        /** Experiment (variant E): no re-submits after writes. */
        @Volatile var noDamage = false
        /** Experiment (variant D): a plain overlay buffer, without the front-buffer flag. */
        @Volatile var noFrontFlag = false
        private val usage: Long get() {
            var u = if (gpuUsage) USAGE or HardwareBuffer.USAGE_GPU_COLOR_OUTPUT or HardwareBuffer.USAGE_GPU_SAMPLED_IMAGE else USAGE
            if (noFrontFlag) u = u and HardwareBuffer.USAGE_FRONT_BUFFER.inv()
            return u
        }

        fun supported(): Boolean = try {
            HardwareBuffer.isSupported(2304, 1440, HardwareBuffer.RGBA_8888, 1, USAGE)
        } catch (_: Throwable) {
            false
        }

        /** Covers [view] with a CPU-written front buffer; null if the device cannot. */
        fun attach(view: SurfaceView): CpuFront? {
            val parent = view.surfaceControl
            if (!parent.isValid || view.width == 0) return null
            // Allocated in the panel's own orientation and rotated back by the layer, so the
            // display hardware scans it out as is (the same as androidx's front buffers).
            val hint = view.rootSurfaceControl?.bufferTransformHint ?: 0
            val swap = hint == SurfaceControl.BUFFER_TRANSFORM_ROTATE_90 || hint == SurfaceControl.BUFFER_TRANSFORM_ROTATE_270
            val (bw, bh) = if (swap) view.height to view.width else view.width to view.height
            val buffer = try {
                HardwareBuffer.create(bw, bh, HardwareBuffer.RGBA_8888, 1, usage)
            } catch (_: Exception) {
                return null
            }
            val handle = Native.frontAttach(buffer, hint)
            if (handle == 0L) {
                buffer.close()
                return null
            }
            val inverse = when (hint) {
                SurfaceControl.BUFFER_TRANSFORM_ROTATE_90 -> SurfaceControl.BUFFER_TRANSFORM_ROTATE_270
                SurfaceControl.BUFFER_TRANSFORM_ROTATE_270 -> SurfaceControl.BUFFER_TRANSFORM_ROTATE_90
                else -> hint
            }
            val sc = SurfaceControl.Builder().setName("tabdisplay-front").setParent(parent).build()
            Native.frontFill(handle, 0, 0, view.width, view.height, 0xff000000.toInt())
            SurfaceControl.Transaction()
                .setBuffer(sc, buffer)
                .setBufferTransform(sc, inverse)
                .setLayer(sc, 1)
                .setVisibility(sc, true)
                .apply()
            val twin = if (singleBuffer) null else try {
                HardwareBuffer.create(bw, bh, HardwareBuffer.RGBA_8888, 1, usage).takeIf { Native.frontAddTwin(handle, it) }
            } catch (_: Exception) {
                null
            }
            android.util.Log.i("tabdisplay", "cpu front ${bw}x$bh, hint $hint, ${if (twin != null) "2 buffers" else "1 buffer"}")
            return CpuFront(handle, sc, buffer, twin)
        }
    }
}
