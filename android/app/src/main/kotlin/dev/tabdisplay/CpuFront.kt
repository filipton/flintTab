package dev.tabdisplay

import android.hardware.HardwareBuffer
import android.view.SurfaceControl
import android.view.SurfaceView

/**
 * A front buffer written by the CPU (Rust, android/native/src/front.rs): one RGBA buffer,
 * attached once to a layer the display hardware scans out directly, then updated in place.
 * Every write is on the panel as soon as its scan line comes by: no GPU, no compositor.
 */
class CpuFront private constructor(val handle: Long, private val sc: SurfaceControl, private val buffer: HardwareBuffer) {
    fun release() {
        SurfaceControl.Transaction().reparent(sc, null).apply()
        sc.release()
        Native.frontRelease(handle)
        buffer.close()
    }

    companion object {
        // Written by the CPU, never read back (the native side keeps its own copy).
        const val USAGE = HardwareBuffer.USAGE_FRONT_BUFFER or HardwareBuffer.USAGE_COMPOSER_OVERLAY or
            HardwareBuffer.USAGE_CPU_WRITE_OFTEN

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
                HardwareBuffer.create(bw, bh, HardwareBuffer.RGBA_8888, 1, USAGE)
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
            android.util.Log.i("tabdisplay", "cpu front ${bw}x$bh, hint $hint")
            return CpuFront(handle, sc, buffer)
        }
    }
}
