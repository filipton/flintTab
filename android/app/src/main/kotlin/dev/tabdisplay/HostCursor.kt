package dev.tabdisplay

import android.graphics.Bitmap
import android.graphics.BitmapFactory
import android.graphics.Paint
import android.graphics.PixelFormat
import android.graphics.Rect
import android.view.Surface
import android.view.SurfaceControl
import android.view.SurfaceView
import kotlin.math.roundToInt

/**
 * Draws the computer's mouse cursor on its own small layer above the video.
 *
 * Moving it is a SurfaceControl position change sent straight to the compositor from the
 * network thread: nothing is redrawn and the UI thread is not involved, so the cursor
 * reaches the screen at the next vsync instead of waiting for capture, encode and decode.
 */
class HostCursor(private val view: SurfaceView) {
    private var layer: SurfaceControl? = null
    private var parent: SurfaceControl? = null
    private var image: Bitmap? = null
    private var displayWidthPt = 0
    private var sizePt = intArrayOf(0, 0)
    private var hotPt = intArrayOf(0, 0)
    private var scale = 1f // tablet pixels per host display point
    private var lastX = 0; private var lastY = 0; private var visible = false

    /** MSG_CURSOR_IMAGE payload. Called on the network thread. */
    @Synchronized
    fun setImage(msg: ByteArray) {
        if (msg.size < 10) return
        fun u16(i: Int) = ((msg[i].toInt() and 0xff) shl 8) or (msg[i + 1].toInt() and 0xff)
        displayWidthPt = u16(0)
        sizePt = intArrayOf(u16(2), u16(4))
        hotPt = intArrayOf(u16(6), u16(8))
        image = BitmapFactory.decodeByteArray(msg, 10, msg.size - 10)
        releaseLayer() // rebuilt at the new size on the next move
        if (visible) move(lastX, lastY, true)
    }

    /** MSG_CURSOR: position as 0..65535 across the display. Called on the network thread. */
    @Synchronized
    fun move(x: Int, y: Int, show: Boolean) {
        lastX = x; lastY = y; visible = show
        val sc = ensureLayer() ?: return
        val px = x / 65535f * view.width - hotPt[0] * scale
        val py = y / 65535f * view.height - hotPt[1] * scale
        SurfaceControl.Transaction()
            .setPosition(sc, px, py)
            .setVisibility(sc, show)
            .apply()
    }

    /** The video surface is going away (its layer, and ours with it). */
    @Synchronized
    fun release() {
        releaseLayer()
    }

    private fun ensureLayer(): SurfaceControl? {
        val p = view.surfaceControl
        if (!p.isValid) { releaseLayer(); return null }
        if (p != parent) releaseLayer()
        layer?.let { return it }
        val bmp = image ?: return null
        if (displayWidthPt == 0 || view.width == 0) return null
        scale = view.width.toFloat() / displayWidthPt
        val w = (sizePt[0] * scale).roundToInt().coerceAtLeast(1)
        val h = (sizePt[1] * scale).roundToInt().coerceAtLeast(1)
        val sc = SurfaceControl.Builder()
            .setName("host-cursor")
            .setParent(p)
            .setBufferSize(w, h)
            .setFormat(PixelFormat.TRANSLUCENT)
            .setOpaque(false)
            .build()
        val surface = Surface(sc)
        try {
            val c = surface.lockHardwareCanvas()
            c.drawColor(0, android.graphics.PorterDuff.Mode.CLEAR)
            c.drawBitmap(bmp, null, Rect(0, 0, w, h), Paint(Paint.FILTER_BITMAP_FLAG))
            surface.unlockCanvasAndPost(c)
        } finally {
            surface.release()
        }
        SurfaceControl.Transaction().setLayer(sc, 1).apply()
        layer = sc
        parent = p
        return sc
    }

    private fun releaseLayer() {
        layer?.let { SurfaceControl.Transaction().reparent(it, null).apply(); it.release() }
        layer = null
        parent = null
    }
}
