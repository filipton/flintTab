package dev.tabdisplay

import java.nio.ByteBuffer
import java.nio.ByteOrder

/** Native hot paths (Rust, android/native). Falls back to Kotlin where the library is missing. */
object Native {
    private val loaded = try {
        System.loadLibrary("tabdisplay_native")
        true
    } catch (_: Throwable) {
        false
    }

    @JvmStatic private external fun lz4(src: ByteArray, off: Int, len: Int, dst: ByteBuffer): Int

    /** LZ4 block data at [off] in [src] into the direct buffer [dst] (limit = bytes written). */
    fun lz4Into(src: ByteArray, off: Int, len: Int, dst: ByteBuffer): Boolean {
        val n = if (loaded) {
            lz4(src, off, len, dst)
        } else {
            val tmp = ByteArray(dst.capacity())
            lz4Decompress(src, off, len, tmp).also { dst.clear(); dst.put(tmp, 0, it) }
        }
        if (n < 0) return false
        dst.position(0).limit(n)
        return true
    }

    @JvmStatic external fun frontAttach(hb: android.hardware.HardwareBuffer, hint: Int): Long
    @JvmStatic external fun frontRelease(handle: Long)
    @JvmStatic external fun probeYuv(hb: android.hardware.HardwareBuffer)
    @JvmStatic external fun yuvCreate(w: Int, h: Int): Long
    @JvmStatic external fun yuvRelease(handle: Long)
    @JvmStatic external fun yuvAddBuffer(handle: Long, hb: android.hardware.HardwareBuffer): Int
    @JvmStatic external fun yuvUpdate(
        handle: Long, y: ByteBuffer, yStride: Int, u: ByteBuffer, v: ByteBuffer, uvStride: Int, uvStep: Int,
        ox: Int, oy: Int, x0: Int, y0: Int, x1: Int, y1: Int,
    )
    @JvmStatic external fun yuvRender(handle: Long, index: Int): Boolean
    @JvmStatic external fun yuvDump(handle: Long, out: ByteBuffer)
    @JvmStatic external fun chainCreate(surface: android.view.Surface, owner: NdkChain): Long
    @JvmStatic external fun chainAddBuffer(chain: Long, hb: android.hardware.HardwareBuffer): Boolean
    @JvmStatic external fun chainTake(chain: Long): Int
    @JvmStatic external fun chainSubmit(chain: Long, index: Int, batch: Long)
    @JvmStatic external fun chainRelease(chain: Long)
    @JvmStatic external fun yuvAttachFront(handle: Long, hb: android.hardware.HardwareBuffer, hint: Int): Boolean
    @JvmStatic external fun yuvVsync(handle: Long, vsyncNanos: Long, periodNanos: Long)
    @JvmStatic external fun yuvPresent(handle: Long, frame: Boolean): Boolean
    @JvmStatic external fun yuvCountFrame(handle: Long)
    @JvmStatic external fun yuvCursorImage(handle: Long, rgba: ByteBuffer, w: Int, h: Int)
    @JvmStatic external fun yuvCursorMove(handle: Long, x: Int, y: Int, shown: Boolean)
    @JvmStatic external fun frontAddChainBuffer(handle: Long, hb: android.hardware.HardwareBuffer): Int
    @JvmStatic external fun frontRenderChain(handle: Long, index: Int): Boolean
    @JvmStatic external fun frontAddTwin(handle: Long, hb: android.hardware.HardwareBuffer): Boolean
    @JvmStatic external fun frontFill(handle: Long, x0: Int, y0: Int, x1: Int, y1: Int, rgba: Int)
    @JvmStatic external fun frontYuv(
        handle: Long, y: ByteBuffer, yStride: Int, u: ByteBuffer, v: ByteBuffer, uvStride: Int, uvStep: Int,
        ox: Int, oy: Int, x0: Int, y0: Int, x1: Int, y1: Int,
    ): Boolean
    @JvmStatic external fun frontPresent(handle: Long): Boolean
    @JvmStatic external fun frontCountFrame(handle: Long)
    @JvmStatic external fun frontVsync(handle: Long, vsyncNanos: Long, periodNanos: Long)
    @JvmStatic external fun frontCursorImage(handle: Long, rgba: ByteBuffer, w: Int, h: Int)
    @JvmStatic external fun frontCursorMove(handle: Long, x: Int, y: Int, shown: Boolean)
    @JvmStatic external fun frontDump(handle: Long, out: ByteBuffer)
    @JvmStatic external fun frontDumpScanout(handle: Long, out: ByteBuffer): Boolean

    /**
     * Free buffers. Removal must be by identity: ByteBuffer.equals compares contents, so a
     * collection's remove(b) could take out another buffer with the same bytes and leave b
     * in the pool, to be handed out again while in use. That gave a tile's luma and chroma
     * the same buffer: the chroma overwrote the upper half of the luma, gray bars on screen.
     */
    private val pool = ArrayDeque<ByteBuffer>()

    /** A direct buffer of at least [size] bytes from the pool; give it back with [recycle]. */
    fun buffer(size: Int): ByteBuffer {
        synchronized(pool) {
            val it = pool.iterator()
            while (it.hasNext()) {
                val b = it.next()
                if (b.capacity() >= size) {
                    it.remove()
                    return b.also { b.clear().limit(size) }
                }
            }
        }
        // Rounded up so buffers fit later tiles of a similar size.
        val cap = (size + 65535) and 65535.inv()
        return ByteBuffer.allocateDirect(cap).order(ByteOrder.nativeOrder()).also { it.limit(size) }
    }

    fun recycle(b: ByteBuffer) {
        synchronized(pool) {
            if (pool.size < 16 && pool.none { it === b }) pool.add(b)
        }
    }
}
