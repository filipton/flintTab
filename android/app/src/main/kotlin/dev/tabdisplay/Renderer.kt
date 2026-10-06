package dev.tabdisplay

import android.graphics.Bitmap
import android.view.Surface
import java.nio.ByteBuffer

/** Where the computer's screen is drawn when front-buffered: [CpuRenderer] or [FrontRenderer]. */
interface Renderer {
    /** The decoder's output surface; null until ready. */
    val surface: Surface?

    /** Call once the view has a surface. */
    fun start()

    /** A new stream of [w] x [h] pixels. */
    fun configure(w: Int, h: Int)

    /** Video frame [pts] went into the decoder; it is drawn once decoded, in order. */
    fun queueVideo(pts: Long)

    /** Exact NV12 pixels for the area at ([x], [y]), drawn right after the updates before it. */
    fun queueTile(pts: Long, x: Int, y: Int, w: Int, h: Int, luma: ByteBuffer, chroma: ByteBuffer)

    fun setCursorImage(bitmap: Bitmap?, displayWidthPt: Int, sizePt: IntArray, hotPt: IntArray)

    fun moveCursor(x: Int, y: Int, shown: Boolean)

    fun release()

    /** True: decode into CPU-readable images handed to [frameDecoded] instead of [surface]. */
    val wantsImages: Boolean get() = false

    /** A decoded frame (with [wantsImages]); call [done] once it is drawn. */
    fun frameDecoded(pts: Long, picture: Picture, done: () -> Unit) = done()
}

/**
 * A decoded YUV 4:2:0 picture the CPU can read: Y, and Cb / Cr ([uvStep] 2: interleaved, so
 * [v] is [u] one byte on), over the decoder's own memory until it is given back. [crop]: the
 * part that is the picture.
 */
class Picture(
    val y: ByteBuffer,
    val u: ByteBuffer,
    val v: ByteBuffer,
    val yStride: Int,
    val uvStride: Int,
    val uvStep: Int,
    val crop: android.graphics.Rect,
) {
    companion object {
        fun of(img: android.media.Image): Picture {
            val p = img.planes
            return Picture(p[0].buffer, p[1].buffer, p[2].buffer, p[0].rowStride, p[1].rowStride, p[1].pixelStride, img.cropRect)
        }
    }
}
