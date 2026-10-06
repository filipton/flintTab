package dev.tabdisplay

import android.hardware.HardwareBuffer
import android.os.Handler
import android.view.SurfaceView

/** A swap chain of NV12 buffers the compositor shows ([YuvChain] on Android 13+, [NdkChain] before). */
interface Chain {
    /** The NV12 screen (yuv.rs) that writes the buffers. */
    val handle: Long

    /**
     * Brings a free buffer up to date and hands it to the compositor; [onShown] gets the time
     * it reached the screen. False if every buffer is still with the compositor: [retry] runs
     * once one comes back. Render thread only.
     */
    fun submit(onShown: (Long) -> Unit, retry: () -> Unit): Boolean

    fun release()
}

/**
 * [YuvChain] for Android 11 and 12, where SurfaceControl.Transaction.setBuffer does not exist
 * yet: the same NV12 buffers, handed to the compositor through the NDK's ASurfaceControl
 * (android/native/src/chain.rs), which those versions have.
 */
class NdkChain private constructor(
    override val handle: Long,
    private val buffers: List<HardwareBuffer>,
    private val handler: Handler,
) : Chain {
    private var chain = 0L
    private var batch = 0L
    private val waiting = HashMap<Long, (Long) -> Unit>()
    private var pendingRetry: (() -> Unit)? = null

    /**
     * A frame handed over and not yet on screen. Only one at a time: a second one in the same
     * refresh would replace it unseen (a dropped frame: video stutters), and queue latency.
     * What comes meanwhile goes into the next one.
     */
    @Volatile private var inFlight = false

    override fun submit(onShown: (Long) -> Unit, retry: () -> Unit): Boolean {
        if (inFlight) {
            pendingRetry = retry
            return false
        }
        val i = Native.chainTake(chain)
        if (i < 0) {
            pendingRetry = retry
            return false
        }
        Native.yuvRender(handle, i)
        val b = ++batch
        synchronized(waiting) { waiting[b] = onShown }
        inFlight = true
        Native.chainSubmit(chain, i, b)
        return true
    }

    /** From the compositor's thread (chain.rs): batch [b] was latched at [nanos]. */
    @Suppress("unused")
    fun presented(b: Long, nanos: Long) {
        val f = synchronized(waiting) { waiting.remove(b) }
        if (!commitCallbacks) letNextGo()
        f?.invoke(if (nanos > 0) nanos else System.nanoTime())
    }

    /** From the compositor's thread (Android 12+): it took the frame for its next refresh. */
    @Suppress("unused")
    fun committed() {
        commitCallbacks = true
        letNextGo()
    }

    /** Whether commits are reported (else a frame counts as taken once it was shown). */
    @Volatile private var commitCallbacks = android.os.Build.VERSION.SDK_INT >= 31

    private fun letNextGo() {
        inFlight = false
        handler.post {
            pendingRetry?.let {
                pendingRetry = null
                it()
            }
        }
    }

    /** From the compositor's thread: a buffer came back. */
    @Suppress("unused")
    fun freed() {
        handler.post {
            pendingRetry?.let {
                pendingRetry = null
                it()
            }
        }
    }

    override fun release() {
        if (chain != 0L) Native.chainRelease(chain)
        chain = 0L
        Native.yuvRelease(handle)
        buffers.forEach { it.close() }
    }

    companion object {
        private const val USAGE = HardwareBuffer.USAGE_COMPOSER_OVERLAY or HardwareBuffer.USAGE_GPU_SAMPLED_IMAGE or
            HardwareBuffer.USAGE_CPU_WRITE_RARELY

        fun supported(): Boolean = android.os.Build.VERSION.SDK_INT >= 29 && try {
            HardwareBuffer.isSupported(1920, 1080, HardwareBuffer.YCBCR_420_888, 1, USAGE)
        } catch (_: Throwable) {
            false
        }

        /** Four [bw] x [bh] buffers (the stream's size), scaled to [view]; null if not ready or failed. */
        fun attach(view: SurfaceView, handler: Handler, bw: Int, bh: Int): NdkChain? {
            val surface = view.holder.surface
            if (surface == null || !surface.isValid || view.width == 0) return null
            val (w, h) = bw and 1.inv() to (bh and 1.inv())
            val buffers = try {
                List(4) { HardwareBuffer.create(w, h, HardwareBuffer.YCBCR_420_888, 1, USAGE) }
            } catch (e: Exception) {
                TLog.i("NV12 chain (NDK): cannot allocate buffers: ${e.message}")
                return null
            }
            val handle = Native.yuvCreate(w, h)
            val c = NdkChain(handle, buffers, handler)
            c.chain = Native.chainCreate(surface, c, w, h, view.width, view.height)
            val ok = c.chain != 0L && buffers.all { Native.yuvAddBuffer(handle, it) >= 0 && Native.chainAddBuffer(c.chain, it) }
            if (!ok) {
                TLog.i("NV12 chain (NDK): the compositor layer could not be set up")
                c.release()
                return null
            }
            TLog.i("NV12 chain (NDK, Android ${android.os.Build.VERSION.SDK_INT}): 4 buffers ${w}x$h, shown at ${view.width}x${view.height}")
            return c
        }
    }
}
