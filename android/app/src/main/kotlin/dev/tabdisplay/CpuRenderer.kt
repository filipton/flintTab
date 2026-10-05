package dev.tabdisplay

import android.graphics.Bitmap
import android.media.Image
import android.os.Handler
import android.os.HandlerThread
import android.os.Process
import android.view.Surface
import android.view.SurfaceView
import java.nio.ByteBuffer
import java.nio.ByteOrder
import java.util.concurrent.ConcurrentLinkedQueue

/**
 * Draws the screen with the CPU straight into the buffer the panel scans out ([CpuFront]):
 * tiles and decoded video frames are converted from YUV by native code, the cursor is a sprite.
 * No GPU and no compositor between an update arriving and the panel showing it.
 *
 * Video frames are decoded into an [ImageReader] the CPU can read; updates are applied in
 * arrival order on one render thread, so an older frame never paints over a newer tile.
 */
class CpuRenderer(
    private val view: SurfaceView,
    /** The area (x0, y0, x1, y1 as 0..65535) video frame `pts` changed; null redraws everything. */
    private val changedArea: (pts: Long) -> IntArray?,
    private val onShown: (pts: Long, nanos: Long) -> Unit,
) : Renderer {
    private val thread = HandlerThread("render", Process.THREAD_PRIORITY_URGENT_DISPLAY).apply { start() }
    private val handler = Handler(thread.looper)

    /**
     * Tells the kernel how long a screen update may take, so it keeps the CPU clocked for it:
     * left alone, the cores idle at low frequency between updates and the same update takes
     * up to 5x longer.
     */
    private var hint: android.os.PerformanceHintManager.Session? = null

    private fun reportWork(started: Long) {
        try {
            hint?.reportActualWorkDuration(System.nanoTime() - started)
        } catch (_: Exception) {}
    }
    private var front: CpuFront? = null
    /** Only to vote for the panel's refresh rate: frames come in as images. */
    @Volatile override var surface: Surface? = null
        private set
    override val wantsImages get() = true

    private sealed class Update(val pts: Long, val queuedAt: Long = System.nanoTime())
    private class Video(pts: Long) : Update(pts)
    private class Tile(pts: Long, val x: Int, val y: Int, val w: Int, val h: Int, val luma: ByteBuffer, val chroma: ByteBuffer) : Update(pts)

    private val updates = ConcurrentLinkedQueue<Update>()
    /** Decoded frames by pts with how to give them back, waiting for their turn (render thread only). */
    private val frames = HashMap<Long, Pair<Image, () -> Unit>>()
    private var streamW = 0
    private var streamH = 0
    private var fullNext = true
    private var dumpBuf: ByteBuffer? = null

    // Cursor (network thread writes, render thread draws)
    @Volatile private var cursorImage: Bitmap? = null
    private var cursorUploaded: Bitmap? = null
    @Volatile private var cursorScale = 1f
    @Volatile private var cursorHot = intArrayOf(0, 0)
    @Volatile private var cursorX = 0
    @Volatile private var cursorY = 0
    @Volatile private var cursorShown = false

    override fun start() {
        handler.post(::attach)
    }

    private fun attach() {
        if (front != null) return
        val f = CpuFront.attach(view)
        if (f == null) {
            handler.postDelayed(::attach, 100) // the view is not laid out yet
            return
        }
        front = f
        surface = view.holder.surface
        hint = try {
            view.context.getSystemService(android.os.PerformanceHintManager::class.java)
                ?.createHintSession(intArrayOf(Process.myTid()), 1_000_000L) // 1 ms per update
        } catch (_: Exception) {
            null
        }
    }

    override fun frameDecoded(pts: Long, image: Image, done: () -> Unit) {
        handler.post {
            frames.put(pts, image to done)?.second?.invoke()
            process()
        }
    }

    override fun configure(w: Int, h: Int) {
        handler.post {
            streamW = w
            streamH = h
            fullNext = true
            while (true) {
                val u = updates.poll() ?: break
                if (u is Tile) { Native.recycle(u.luma); Native.recycle(u.chroma) }
            }
            frames.values.forEach { it.second() }
            frames.clear()
        }
    }

    override fun queueVideo(pts: Long) {
        updates.add(Video(pts))
    }

    override fun queueTile(pts: Long, x: Int, y: Int, w: Int, h: Int, luma: ByteBuffer, chroma: ByteBuffer) {
        updates.add(Tile(pts, x, y, w, h, luma, chroma))
        handler.post(::process)
    }

    override fun setCursorImage(bitmap: Bitmap?, displayWidthPt: Int, sizePt: IntArray, hotPt: IntArray) {
        if (bitmap == null || displayWidthPt == 0) return
        val scale = view.width.toFloat() / displayWidthPt
        // Scaled to the panel once here; the native sprite is drawn 1:1.
        val w = maxOf(1, (sizePt[0] * scale).toInt())
        val h = maxOf(1, (sizePt[1] * scale).toInt())
        cursorHot = intArrayOf((hotPt[0] * scale).toInt(), (hotPt[1] * scale).toInt())
        cursorScale = scale
        cursorImage = Bitmap.createScaledBitmap(bitmap.copy(Bitmap.Config.ARGB_8888, false), w, h, true)
        handler.post(::drawCursor)
    }

    override fun moveCursor(x: Int, y: Int, shown: Boolean) {
        cursorX = x; cursorY = y; cursorShown = shown
        handler.post(::drawCursor)
    }

    private fun drawCursor() {
        val f = front ?: return
        val img = cursorImage
        if (img != null && img !== cursorUploaded) {
            val buf = ByteBuffer.allocateDirect(img.byteCount).order(ByteOrder.nativeOrder())
            img.copyPixelsToBuffer(buf) // premultiplied RGBA
            Native.frontCursorImage(f.handle, buf, img.width, img.height)
            cursorUploaded = img
        }
        val x = cursorX / 65535f * view.width - cursorHot[0]
        val y = cursorY / 65535f * view.height - cursorHot[1]
        Native.frontCursorMove(f.handle, x.toInt(), y.toInt(), cursorShown && img != null)
    }

    /** Applies queued updates in order until one is a video frame still being decoded. */
    private fun process() {
        val f = front ?: return
        val started = System.nanoTime()
        try {
            processUpdates(f)
        } finally {
            reportWork(started)
        }
    }

    private fun processUpdates(f: CpuFront) {
        while (true) {
            val u = updates.peek() ?: break
            when (u) {
                is Tile -> {
                    Native.frontYuv(f.handle, u.luma, u.w, u.chroma, u.chroma.duplicate().also { it.position(1) }.slice(),
                        u.w, 2, u.x, u.y, u.x, u.y, u.x + u.w, u.y + u.h)
                    Native.recycle(u.luma)
                    Native.recycle(u.chroma)
                    onShown(u.pts, System.nanoTime())
                    updates.poll()
                }
                is Video -> {
                    val frame = frames.remove(u.pts)
                    when {
                        frame != null -> {
                            drawFrame(f, frame.first, u.pts)
                            frame.second()
                            onShown(u.pts, System.nanoTime())
                            updates.poll()
                        }
                        frames.keys.any { it > u.pts } || System.nanoTime() - u.queuedAt > 300_000_000L -> updates.poll() // lost
                        else -> {
                            handler.postDelayed(::process, 50) // in case it never comes
                            return
                        }
                    }
                }
            }
        }
        // Frames nobody waits for (should not happen): drop them.
        if (updates.isEmpty() && frames.isNotEmpty()) {
            frames.values.forEach { it.second() }
            frames.clear()
        }
    }

    private fun drawFrame(f: CpuFront, img: Image, pts: Long) {
        val w = minOf(img.width, view.width)
        val h = minOf(img.height, view.height)
        val c = if (fullNext) null else changedArea(pts)
        fullNext = false
        val x0: Int; val y0: Int; val x1: Int; val y1: Int
        if (c == null) {
            x0 = 0; y0 = 0; x1 = w; y1 = h
        } else {
            // Padded by 2 px: chroma is shared between pixel pairs.
            x0 = maxOf(0, (c[0] / 65535f * w).toInt() - 2); y0 = maxOf(0, (c[1] / 65535f * h).toInt() - 2)
            x1 = minOf(w, Math.ceil(c[2] / 65535.0 * w).toInt() + 2); y1 = minOf(h, Math.ceil(c[3] / 65535.0 * h).toInt() + 2)
        }
        if (x0 >= x1 || y0 >= y1) return
        val p = img.planes
        Native.frontYuv(f.handle, p[0].buffer, p[0].rowStride, p[1].buffer, p[2].buffer, p[1].rowStride, p[1].pixelStride,
            0, 0, x0, y0, x1, y1)
    }

    /** Saves what the panel shows to [path] as a PNG (debugging). */
    fun dump(path: java.io.File) {
        handler.post {
            val f = front ?: return@post
            val buf = dumpBuf ?: ByteBuffer.allocateDirect(view.width * view.height * 4).order(ByteOrder.nativeOrder()).also { dumpBuf = it }
            buf.clear()
            Native.frontDump(f.handle, buf)
            val bmp = Bitmap.createBitmap(view.width, view.height, Bitmap.Config.ARGB_8888)
            buf.rewind()
            bmp.copyPixelsFromBuffer(buf)
            path.outputStream().use { bmp.compress(Bitmap.CompressFormat.PNG, 100, it) }
            android.util.Log.i("tabdisplay", "dumped the front buffer to $path")
        }
    }

    override fun release() {
        handler.post {
            frames.values.forEach { it.second() }
            front?.release()
            front = null
        }
        thread.quitSafely()
    }
}
