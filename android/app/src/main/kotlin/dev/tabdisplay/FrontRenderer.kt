package dev.tabdisplay

import android.graphics.Bitmap
import android.graphics.SurfaceTexture
import android.hardware.HardwareBuffer
import android.opengl.GLES11Ext
import android.opengl.GLES20
import android.opengl.GLUtils
import android.opengl.Matrix
import android.os.Handler
import android.os.HandlerThread
import android.os.Process
import android.view.Surface
import android.view.SurfaceView
import androidx.graphics.lowlatency.BufferInfo
import androidx.graphics.lowlatency.GLFrontBufferedRenderer
import androidx.graphics.opengl.egl.EGLManager
import java.nio.ByteBuffer
import java.nio.ByteOrder
import java.nio.FloatBuffer
import java.util.concurrent.ConcurrentLinkedQueue
import java.util.concurrent.atomic.AtomicInteger
import java.util.concurrent.atomic.AtomicLong

/**
 * Draws the computer's screen and cursor straight into the buffer the panel is scanning out
 * (front-buffered rendering, as stylus apps do), instead of queueing frames to SurfaceFlinger.
 *
 * SurfaceFlinger latches a queued buffer at its next wake-up and shows it one refresh later
 * (~17 ms on average at 90 Hz). Here an update is visible as soon as the GPU has drawn it and
 * the panel's scan reaches it. The price is that an update can tear while it is drawn.
 *
 * The screen is kept in a canvas texture, updated in arrival order by video frames (decoded
 * into a SurfaceTexture, only their changed area) and by tiles (exact pixels sent without the
 * codec). Each update, and each cursor move, then copies just the affected area to the panel.
 */
class FrontRenderer(
    view: SurfaceView,
    /** The area (x0, y0, x1, y1 as 0..65535) video frame `pts` changed; null redraws everything. */
    private val changedArea: (pts: Long) -> IntArray?,
    private val onShown: (pts: Long, nanos: Long) -> Unit,
) : Renderer {
    /** The decoder's output surface; null until the GL thread has created it. */
    @Volatile override var surface: Surface? = null
        private set

    private val renderer = GLFrontBufferedRenderer(view, Callbacks())
    private val events = HandlerThread("frame-available", Process.THREAD_PRIORITY_URGENT_DISPLAY).apply { start() }
    private val eventHandler = Handler(events.looper)
    /** When the outstanding draw was requested (ns), 0 if none. */
    private val drawRequested = AtomicLong(0)

    private sealed class Update(val pts: Long, val queuedAt: Long = System.nanoTime())
    private class Video(pts: Long) : Update(pts)
    private class Tile(pts: Long, val x: Int, val y: Int, val w: Int, val h: Int, val luma: ByteBuffer, val chroma: ByteBuffer) : Update(pts)

    /** Screen updates in arrival order; a video frame holds back what follows until it is decoded. */
    private val updates = ConcurrentLinkedQueue<Update>()

    // GL thread state
    private var texture: SurfaceTexture? = null
    private var videoTex = 0
    private var cursorTex = 0
    private var canvasTex = 0
    private var canvasFbo = 0
    private var lumaTex = 0
    private var chromaTex = 0
    private var videoProg = 0
    private var tileProg = 0
    private var plainProg = 0
    private var canvasW = 0
    private var canvasH = 0
    @Volatile private var streamW = 0
    @Volatile private var streamH = 0
    /** Decoded frames queued in the SurfaceTexture and not latched yet. */
    private val newFrames = AtomicInteger(0)
    private var latchedPts = -1L
    private val stMatrix = FloatArray(16)
    private val mvp = FloatArray(16)
    private val proj = FloatArray(16)
    private val quad: FloatBuffer = ByteBuffer.allocateDirect(16 * 4).order(ByteOrder.nativeOrder()).asFloatBuffer()
    private val changed = Area() // canvas pixels
    private val area = Area() // view pixels
    private var cursorDrawn: FloatArray? = null
    private var redrawAll = true
    private val shown = ArrayList<Long>()
    private val corner = FloatArray(4)
    private val mapped = FloatArray(4)

    // Cursor, written by the network thread
    @Volatile private var cursorImage: Bitmap? = null
    @Volatile private var cursorUploaded: Bitmap? = null
    @Volatile private var cursorX = 0
    @Volatile private var cursorY = 0
    @Volatile private var cursorShown = false
    @Volatile private var displayWidthPt = 0
    @Volatile private var cursorSizePt = intArrayOf(0, 0)
    @Volatile private var cursorHotPt = intArrayOf(0, 0)

    /**
     * Call once the view has a surface: creates the decoder's output surface on the GL thread.
     * The renderer drops draws until its own surface setup is done, so retry until it exists.
     */
    override fun start() {
        if (surface != null) return
        drawRequested.set(0)
        requestDraw()
        eventHandler.postDelayed({ start() }, 100)
    }

    /** A new stream of [w] x [h] pixels: everything is redrawn from its first frame. */
    override fun configure(w: Int, h: Int) {
        streamW = w
        streamH = h
        while (true) {
            val u = updates.poll() ?: break
            if (u is Tile) { Native.recycle(u.luma); Native.recycle(u.chroma) }
        }
    }

    /** Video frame [pts] went into the decoder; it is drawn once decoded, in order. */
    override fun queueVideo(pts: Long) {
        updates.add(Video(pts))
    }

    /** Exact NV12 pixels for the area at ([x], [y]), drawn right after the updates before it. */
    override fun queueTile(pts: Long, x: Int, y: Int, w: Int, h: Int, luma: ByteBuffer, chroma: ByteBuffer) {
        updates.add(Tile(pts, x, y, w, h, luma, chroma))
        requestDraw()
    }

    override fun setCursorImage(bitmap: Bitmap?, displayWidthPt: Int, sizePt: IntArray, hotPt: IntArray) {
        this.displayWidthPt = displayWidthPt
        cursorSizePt = sizePt
        cursorHotPt = hotPt
        cursorImage = bitmap
        requestDraw()
    }

    override fun moveCursor(x: Int, y: Int, shown: Boolean) {
        if (x == cursorX && y == cursorY && shown == cursorShown) return
        cursorX = x; cursorY = y; cursorShown = shown
        requestDraw()
    }

    override fun release() {
        renderer.release(true)
        events.quitSafely()
    }

    /**
     * One draw at a time; whatever changes meanwhile is picked up by the pending draw. A request
     * the renderer dropped (surface not ready) must not block later ones, hence the timeout.
     */
    private fun requestDraw() {
        val now = System.nanoTime()
        val since = drawRequested.get()
        if ((since == 0L || now - since > 100_000_000L) && drawRequested.compareAndSet(since, now)) {
            renderer.renderFrontBufferedLayer(Unit)
        }
    }

    private inner class Callbacks : GLFrontBufferedRenderer.Callback<Unit> {
        override fun onDrawFrontBufferedLayer(
            eglManager: EGLManager, width: Int, height: Int, bufferInfo: BufferInfo, transform: FloatArray, param: Unit,
        ) {
            drawRequested.set(0)
            if (texture == null) init()
            val w = width.toFloat()
            val h = height.toFloat()
            if (streamW > 0 && (canvasW != streamW || canvasH != streamH)) makeCanvas(streamW, streamH)

            // 1. Apply updates to the canvas, in order.
            changed.clear()
            shown.clear()
            if (canvasFbo != 0) applyUpdates()

            // 2. Copy what changed, and where the cursor was and is, to the panel.
            area.clear()
            if (!changed.isEmpty()) {
                val sx = w / canvasW
                val sy = h / canvasH
                area.add(changed.x0 * sx, changed.y0 * sy, changed.x1 * sx, changed.y1 * sy)
            }
            val img = cursorImage
            val cursor = if (cursorShown && img != null && displayWidthPt > 0) {
                val scale = w / displayWidthPt
                val x = cursorX / 65535f * w - cursorHotPt[0] * scale
                val y = cursorY / 65535f * h - cursorHotPt[1] * scale
                floatArrayOf(x, y, x + cursorSizePt[0] * scale, y + cursorSizePt[1] * scale)
            } else null
            if (!cursor.contentEquals(cursorDrawn) || img !== cursorUploaded) {
                cursorDrawn?.let { area.add(it[0], it[1], it[2], it[3]) }
                cursor?.let { area.add(it[0], it[1], it[2], it[3]) }
            }
            if (redrawAll && canvasFbo != 0) {
                area.add(0f, 0f, w, h)
                redrawAll = false
            }
            if (!area.isEmpty() && canvasFbo != 0) {
                GLES20.glBindFramebuffer(GLES20.GL_FRAMEBUFFER, bufferInfo.frameBufferId)
                GLES20.glViewport(0, 0, bufferInfo.width, bufferInfo.height)
                Matrix.orthoM(proj, 0, 0f, bufferInfo.width.toFloat(), 0f, bufferInfo.height.toFloat(), -1f, 1f)
                Matrix.multiplyMM(mvp, 0, proj, 0, transform, 0)
                scissor(area, w, h, transform)
                GLES20.glDisable(GLES20.GL_BLEND)
                draw(plainProg, GLES20.GL_TEXTURE_2D, canvasTex, 0f, 0f, w, h, 0f, 0f, 1f, 1f, IDENTITY)
                if (cursor != null) {
                    if (cursorUploaded !== img) {
                        GLES20.glBindTexture(GLES20.GL_TEXTURE_2D, cursorTex)
                        GLUtils.texImage2D(GLES20.GL_TEXTURE_2D, 0, img, 0)
                        cursorUploaded = img
                    }
                    GLES20.glEnable(GLES20.GL_BLEND)
                    GLES20.glBlendFunc(GLES20.GL_ONE, GLES20.GL_ONE_MINUS_SRC_ALPHA) // bitmaps are premultiplied
                    draw(plainProg, GLES20.GL_TEXTURE_2D, cursorTex, cursor[0], cursor[1], cursor[2], cursor[3], 0f, 0f, 1f, 1f, IDENTITY)
                }
                GLES20.glDisable(GLES20.GL_SCISSOR_TEST)
                cursorDrawn = cursor
            }
            if (shown.isNotEmpty()) {
                GLES20.glFinish() // the update is in the scanned-out buffer from here on
                val now = System.nanoTime()
                for (p in shown) onShown(p, now)
            }
        }

        override fun onDrawMultiBufferedLayer(
            eglManager: EGLManager, width: Int, height: Int, bufferInfo: BufferInfo, transform: FloatArray, params: Collection<Unit>,
        ) {}
    }

    /** Draws queued updates into the canvas until one is a video frame still being decoded. */
    private fun applyUpdates() {
        GLES20.glBindFramebuffer(GLES20.GL_FRAMEBUFFER, canvasFbo)
        GLES20.glViewport(0, 0, canvasW, canvasH)
        Matrix.orthoM(mvp, 0, 0f, canvasW.toFloat(), 0f, canvasH.toFloat(), -1f, 1f)
        GLES20.glDisable(GLES20.GL_BLEND)
        val st = texture!!
        while (true) {
            val u = updates.peek()
            if (u == null) {
                // Frames nobody queued (should not happen): show them anyway.
                while (newFrames.get() > 0) {
                    latch(st)
                    drawVideo(null)
                }
                break
            }
            when (u) {
                is Tile -> {
                    drawTile(u)
                    changed.add(u.x.toFloat(), u.y.toFloat(), (u.x + u.w).toFloat(), (u.y + u.h).toFloat())
                    shown.add(u.pts)
                    updates.poll()
                }
                is Video -> {
                    while (latchedPts < u.pts && newFrames.get() > 0) latch(st)
                    when {
                        latchedPts == u.pts -> {
                            drawVideo(changedArea(u.pts))
                            shown.add(u.pts)
                            updates.poll()
                        }
                        latchedPts > u.pts || System.nanoTime() - u.queuedAt > 300_000_000L -> updates.poll() // lost
                        else -> {
                            // Still decoding: its frame-available wakes us; this is for a lost one.
                            eventHandler.postDelayed({ requestDraw() }, 50)
                            return
                        }
                    }
                }
            }
        }
    }

    private fun latch(st: SurfaceTexture) {
        newFrames.decrementAndGet()
        st.updateTexImage()
        st.getTransformMatrix(stMatrix)
        latchedPts = st.timestamp / 1000
    }

    /** The latched video frame's changed area (0..65535, null = all) into the canvas. */
    private fun drawVideo(c: IntArray?) {
        val cw = canvasW.toFloat()
        val ch = canvasH.toFloat()
        val r = if (c == null) floatArrayOf(0f, 0f, cw, ch)
        else floatArrayOf(c[0] / 65535f * cw, c[1] / 65535f * ch, c[2] / 65535f * cw, c[3] / 65535f * ch)
        if (r[0] >= r[2] || r[1] >= r[3]) return
        // Padded by 2 px: chroma is shared between pixel pairs.
        val x0 = maxOf(0, r[0].toInt() - 2); val y0 = maxOf(0, r[1].toInt() - 2)
        val x1 = minOf(canvasW, Math.ceil(r[2].toDouble()).toInt() + 2); val y1 = minOf(canvasH, Math.ceil(r[3].toDouble()).toInt() + 2)
        GLES20.glEnable(GLES20.GL_SCISSOR_TEST)
        GLES20.glScissor(x0, y0, x1 - x0, y1 - y0)
        draw(videoProg, GLES11Ext.GL_TEXTURE_EXTERNAL_OES, videoTex, 0f, 0f, cw, ch, 0f, 1f, 1f, 0f, stMatrix)
        GLES20.glDisable(GLES20.GL_SCISSOR_TEST)
        changed.add(x0.toFloat(), y0.toFloat(), x1.toFloat(), y1.toFloat())
    }

    /**
     * The tile's pixels go into the top-left corner of plane textures allocated once at the
     * stream size: (re)allocating a texture per tile costs ~1-2 ms through ANGLE.
     */
    private fun drawTile(t: Tile) {
        GLES20.glPixelStorei(GLES20.GL_UNPACK_ALIGNMENT, 1)
        GLES20.glActiveTexture(GLES20.GL_TEXTURE1)
        GLES20.glBindTexture(GLES20.GL_TEXTURE_2D, chromaTex)
        GLES20.glTexSubImage2D(GLES20.GL_TEXTURE_2D, 0, 0, 0, t.w / 2, t.h / 2,
            GLES20.GL_LUMINANCE_ALPHA, GLES20.GL_UNSIGNED_BYTE, t.chroma)
        GLES20.glUseProgram(tileProg)
        GLES20.glUniform1i(uniform(tileProg, "uC"), 1)
        GLES20.glActiveTexture(GLES20.GL_TEXTURE0)
        GLES20.glBindTexture(GLES20.GL_TEXTURE_2D, lumaTex)
        GLES20.glTexSubImage2D(GLES20.GL_TEXTURE_2D, 0, 0, 0, t.w, t.h,
            GLES20.GL_LUMINANCE, GLES20.GL_UNSIGNED_BYTE, t.luma)
        draw(tileProg, GLES20.GL_TEXTURE_2D, lumaTex, t.x.toFloat(), t.y.toFloat(), (t.x + t.w).toFloat(), (t.y + t.h).toFloat(),
            0f, 0f, t.w.toFloat() / canvasW, t.h.toFloat() / canvasH, IDENTITY)
        // glTexImage2D copied the pixels: the buffers can take the next tile.
        Native.recycle(t.luma)
        Native.recycle(t.chroma)
    }

    private fun makeCanvas(w: Int, h: Int) {
        if (canvasFbo != 0) {
            GLES20.glDeleteFramebuffers(1, intArrayOf(canvasFbo), 0)
            GLES20.glDeleteTextures(1, intArrayOf(canvasTex), 0)
        }
        val ids = IntArray(1)
        GLES20.glGenTextures(1, ids, 0)
        canvasTex = ids[0]
        GLES20.glBindTexture(GLES20.GL_TEXTURE_2D, canvasTex)
        texParams(GLES20.GL_TEXTURE_2D, GLES20.GL_NEAREST)
        GLES20.glTexImage2D(GLES20.GL_TEXTURE_2D, 0, GLES20.GL_RGBA, w, h, 0, GLES20.GL_RGBA, GLES20.GL_UNSIGNED_BYTE, null)
        GLES20.glGenFramebuffers(1, ids, 0)
        canvasFbo = ids[0]
        GLES20.glBindFramebuffer(GLES20.GL_FRAMEBUFFER, canvasFbo)
        GLES20.glFramebufferTexture2D(GLES20.GL_FRAMEBUFFER, GLES20.GL_COLOR_ATTACHMENT0, GLES20.GL_TEXTURE_2D, canvasTex, 0)
        GLES20.glClearColor(0f, 0f, 0f, 1f)
        GLES20.glClear(GLES20.GL_COLOR_BUFFER_BIT)
        // Tile planes, reused for every tile (no tile is larger than the screen).
        GLES20.glBindTexture(GLES20.GL_TEXTURE_2D, lumaTex)
        GLES20.glTexImage2D(GLES20.GL_TEXTURE_2D, 0, GLES20.GL_LUMINANCE, w, h, 0, GLES20.GL_LUMINANCE, GLES20.GL_UNSIGNED_BYTE, null)
        GLES20.glBindTexture(GLES20.GL_TEXTURE_2D, chromaTex)
        GLES20.glTexImage2D(GLES20.GL_TEXTURE_2D, 0, GLES20.GL_LUMINANCE_ALPHA, w / 2, h / 2, 0,
            GLES20.GL_LUMINANCE_ALPHA, GLES20.GL_UNSIGNED_BYTE, null)
        canvasW = w
        canvasH = h
        redrawAll = true
    }

    private fun init() {
        val ids = IntArray(4)
        GLES20.glGenTextures(4, ids, 0)
        videoTex = ids[0]; cursorTex = ids[1]; lumaTex = ids[2]; chromaTex = ids[3]
        GLES20.glBindTexture(GLES11Ext.GL_TEXTURE_EXTERNAL_OES, videoTex)
        texParams(GLES11Ext.GL_TEXTURE_EXTERNAL_OES, GLES20.GL_NEAREST) // 1:1 pixels, nothing to filter
        GLES20.glBindTexture(GLES20.GL_TEXTURE_2D, cursorTex)
        texParams(GLES20.GL_TEXTURE_2D, GLES20.GL_LINEAR)
        GLES20.glBindTexture(GLES20.GL_TEXTURE_2D, lumaTex)
        texParams(GLES20.GL_TEXTURE_2D, GLES20.GL_NEAREST)
        GLES20.glBindTexture(GLES20.GL_TEXTURE_2D, chromaTex)
        texParams(GLES20.GL_TEXTURE_2D, GLES20.GL_LINEAR)
        GLES20.glDisable(GLES20.GL_DITHER)
        videoProg = program(VERTEX, VIDEO_FRAGMENT)
        tileProg = program(VERTEX, TILE_FRAGMENT)
        plainProg = program(VERTEX, PLAIN_FRAGMENT)
        Matrix.setIdentityM(stMatrix, 0)
        val st = SurfaceTexture(videoTex)
        st.setOnFrameAvailableListener({ newFrames.incrementAndGet(); requestDraw() }, eventHandler)
        texture = st
        surface = Surface(st)
    }

    /** Limits drawing to [a] (view pixels, padded for rounding), in buffer coordinates. */
    private fun scissor(a: Area, w: Float, h: Float, transform: FloatArray) {
        val x0 = (a.x0 - 2).coerceAtLeast(0f); val y0 = (a.y0 - 2).coerceAtLeast(0f)
        val x1 = (a.x1 + 2).coerceAtMost(w); val y1 = (a.y1 + 2).coerceAtMost(h)
        var bx0 = Float.MAX_VALUE; var by0 = Float.MAX_VALUE; var bx1 = -Float.MAX_VALUE; var by1 = -Float.MAX_VALUE
        for ((x, y) in arrayOf(x0 to y0, x1 to y0, x0 to y1, x1 to y1)) {
            corner[0] = x; corner[1] = y; corner[2] = 0f; corner[3] = 1f
            Matrix.multiplyMV(mapped, 0, transform, 0, corner, 0)
            bx0 = minOf(bx0, mapped[0]); by0 = minOf(by0, mapped[1]); bx1 = maxOf(bx1, mapped[0]); by1 = maxOf(by1, mapped[1])
        }
        val sx = Math.floor(bx0.toDouble()).toInt(); val sy = Math.floor(by0.toDouble()).toInt()
        GLES20.glEnable(GLES20.GL_SCISSOR_TEST)
        GLES20.glScissor(sx, sy, Math.ceil(bx1.toDouble()).toInt() - sx, Math.ceil(by1.toDouble()).toInt() - sy)
    }

    private fun texParams(target: Int, filter: Int) {
        GLES20.glTexParameteri(target, GLES20.GL_TEXTURE_MIN_FILTER, filter)
        GLES20.glTexParameteri(target, GLES20.GL_TEXTURE_MAG_FILTER, filter)
        GLES20.glTexParameteri(target, GLES20.GL_TEXTURE_WRAP_S, GLES20.GL_CLAMP_TO_EDGE)
        GLES20.glTexParameteri(target, GLES20.GL_TEXTURE_WRAP_T, GLES20.GL_CLAMP_TO_EDGE)
    }

    /** A textured rectangle from (x0, y0) to (x1, y1) with texture coordinates (u0, v0)..(u1, v1). */
    private fun draw(prog: Int, target: Int, tex: Int, x0: Float, y0: Float, x1: Float, y1: Float,
                     u0: Float, v0: Float, u1: Float, v1: Float, texMatrix: FloatArray) {
        quad.clear()
        quad.put(floatArrayOf(x0, y0, u0, v0, x1, y0, u1, v0, x0, y1, u0, v1, x1, y1, u1, v1)).position(0)
        GLES20.glUseProgram(prog)
        val pos = attrib(prog, "aPos")
        val uv = attrib(prog, "aUv")
        GLES20.glVertexAttribPointer(pos, 2, GLES20.GL_FLOAT, false, 16, quad)
        GLES20.glEnableVertexAttribArray(pos)
        quad.position(2)
        GLES20.glVertexAttribPointer(uv, 2, GLES20.GL_FLOAT, false, 16, quad)
        GLES20.glEnableVertexAttribArray(uv)
        GLES20.glUniformMatrix4fv(uniform(prog, "uMvp"), 1, false, mvp, 0)
        GLES20.glUniformMatrix4fv(uniform(prog, "uTex"), 1, false, texMatrix, 0)
        GLES20.glActiveTexture(GLES20.GL_TEXTURE0)
        GLES20.glBindTexture(target, tex)
        GLES20.glUniform1i(uniform(prog, "uS"), 0)
        GLES20.glDrawArrays(GLES20.GL_TRIANGLE_STRIP, 0, 4)
    }

    private val locations = HashMap<String, Int>()
    private fun uniform(prog: Int, name: String) = locations.getOrPut("u$prog$name") { GLES20.glGetUniformLocation(prog, name) }
    private fun attrib(prog: Int, name: String) = locations.getOrPut("a$prog$name") { GLES20.glGetAttribLocation(prog, name) }

    private fun program(vs: String, fs: String): Int {
        fun shader(type: Int, src: String) = GLES20.glCreateShader(type).also {
            GLES20.glShaderSource(it, src)
            GLES20.glCompileShader(it)
        }
        return GLES20.glCreateProgram().also {
            GLES20.glAttachShader(it, shader(GLES20.GL_VERTEX_SHADER, vs))
            GLES20.glAttachShader(it, shader(GLES20.GL_FRAGMENT_SHADER, fs))
            GLES20.glLinkProgram(it)
        }
    }

    companion object {
        private val IDENTITY = FloatArray(16).also { Matrix.setIdentityM(it, 0) }

        /** Whether this device can scan out a buffer that is being drawn into. */
        fun supported(): Boolean = try {
            HardwareBuffer.isSupported(
                64, 64, HardwareBuffer.RGBA_8888, 1,
                HardwareBuffer.USAGE_FRONT_BUFFER or HardwareBuffer.USAGE_GPU_COLOR_OUTPUT or HardwareBuffer.USAGE_COMPOSER_OVERLAY,
            )
        } catch (_: Throwable) {
            false
        }

        private const val VERTEX = """
            attribute vec4 aPos;
            attribute vec4 aUv;
            uniform mat4 uMvp;
            uniform mat4 uTex;
            varying vec2 vUv;
            void main() {
                gl_Position = uMvp * aPos;
                vUv = (uTex * aUv).xy;
            }
        """
        private const val VIDEO_FRAGMENT = """
            #extension GL_OES_EGL_image_external : require
            precision mediump float;
            uniform samplerExternalOES uS;
            varying vec2 vUv;
            void main() { gl_FragColor = texture2D(uS, vUv); }
        """
        /** NV12 (BT.709, video range, like the encoder's input) to RGB. */
        private const val TILE_FRAGMENT = """
            precision mediump float;
            uniform sampler2D uS;
            uniform sampler2D uC;
            varying vec2 vUv;
            void main() {
                float y = (texture2D(uS, vUv).r - 0.0627) * 1.1644;
                vec2 c = texture2D(uC, vUv).ra - 0.5;
                gl_FragColor = vec4(y + 1.7927 * c.y, y - 0.2132 * c.x - 0.5329 * c.y, y + 2.1124 * c.x, 1.0);
            }
        """
        private const val PLAIN_FRAGMENT = """
            precision mediump float;
            uniform sampler2D uS;
            varying vec2 vUv;
            void main() { gl_FragColor = texture2D(uS, vUv); }
        """
    }
}

/** A growing bounding box. */
private class Area {
    var x0 = 0f; var y0 = 0f; var x1 = 0f; var y1 = 0f
    init { clear() }
    fun clear() { x0 = Float.MAX_VALUE; y0 = Float.MAX_VALUE; x1 = -Float.MAX_VALUE; y1 = -Float.MAX_VALUE }
    fun isEmpty() = x0 >= x1 || y0 >= y1
    fun add(ax0: Float, ay0: Float, ax1: Float, ay1: Float) {
        if (ax0 >= ax1 || ay0 >= ay1) return
        x0 = minOf(x0, ax0); y0 = minOf(y0, ay0); x1 = maxOf(x1, ax1); y1 = maxOf(y1, ay1)
    }
}
