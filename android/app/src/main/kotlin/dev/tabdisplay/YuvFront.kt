package dev.tabdisplay

import android.hardware.DataSpace
import android.hardware.HardwareBuffer
import android.view.SurfaceControl
import android.view.SurfaceView

/**
 * The screen as one NV12 buffer the display scans out directly (front-buffered), written in
 * place by native code (android/native/src/yuv.rs) while the panel's scan is elsewhere.
 *
 * An update is on the panel as soon as it is written and the scan passes it: no compositor
 * queue (~16 ms at 90 Hz). NV12 makes writes plain row copies the display rotates and converts
 * itself, and keeps what the display reads small. The same buffer is handed to the compositor
 * again every refresh, so it keeps the display at full rate (it idles to 30 Hz otherwise).
 */
class YuvFront private constructor(
    val handle: Long,
    private val sc: SurfaceControl,
    private val buffer: HardwareBuffer,
) {
    private val notifier = android.os.HandlerThread("front-refresh").apply { start() }
    private val notifyHandler = android.os.Handler(notifier.looper)
    private val pending = java.util.concurrent.atomic.AtomicBoolean(false)

    /** Once per refresh while streaming: keeps the display active at its full rate. */
    fun refresh() {
        if (!pending.compareAndSet(false, true)) return
        notifyHandler.post {
            pending.set(false)
            try {
                SurfaceControl.Transaction().setBuffer(sc, buffer).apply()
            } catch (_: Throwable) {}
        }
    }

    fun release() {
        notifier.quitSafely()
        SurfaceControl.Transaction().reparent(sc, null).apply()
        sc.release()
        Native.yuvRelease(handle)
        buffer.close()
    }

    companion object {
        private const val USAGE = HardwareBuffer.USAGE_FRONT_BUFFER or HardwareBuffer.USAGE_COMPOSER_OVERLAY or
            HardwareBuffer.USAGE_GPU_SAMPLED_IMAGE or HardwareBuffer.USAGE_CPU_WRITE_RARELY

        fun supported(): Boolean = android.os.Build.VERSION.SDK_INT >= 33 && try {
            HardwareBuffer.isSupported(2304, 1440, HardwareBuffer.YCBCR_420_888, 1, USAGE)
        } catch (_: Throwable) {
            false
        }

        fun attach(view: SurfaceView): YuvFront? {
            val parent = view.surfaceControl
            if (!parent.isValid || view.width == 0) return null
            val (w, h) = view.width and 1.inv() to (view.height and 1.inv())
            val buffer = try {
                HardwareBuffer.create(w, h, HardwareBuffer.YCBCR_420_888, 1, USAGE)
            } catch (_: Exception) {
                return null
            }
            val handle = Native.yuvCreate(w, h)
            // Which way the panel scans the screen: its rotation relative to the view.
            val hint = view.rootSurfaceControl?.bufferTransformHint ?: 0
            if (!Native.yuvAttachFront(handle, buffer, hint)) {
                Native.yuvRelease(handle)
                buffer.close()
                return null
            }
            val sc = SurfaceControl.Builder().setName("tabdisplay-front").setParent(parent).build()
            SurfaceControl.Transaction()
                .setBuffer(sc, buffer)
                .setDataSpace(sc, DataSpace.pack(DataSpace.STANDARD_BT709, DataSpace.TRANSFER_SMPTE_170M, DataSpace.RANGE_LIMITED))
                .setOpaque(sc, true)
                .setLayer(sc, 0)
                .setVisibility(sc, true)
                .apply()
            android.util.Log.i("tabdisplay", "yuv front buffer ${w}x$h, hint $hint")
            return YuvFront(handle, sc, buffer)
        }
    }
}
