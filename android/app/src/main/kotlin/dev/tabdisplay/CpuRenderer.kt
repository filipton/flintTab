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
 * Draws the screen with the CPU into a swap chain the compositor shows ([SwapChain]):
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
    /** The default: one NV12 buffer the panel scans out, written in place ([YuvFront]). */
    private var yuvFront: FrontBuffer? = null
    /** Without "lowest latency": NV12 buffers the compositor shows ([YuvChain]); cursor on its own layer. */
    private var yuv: Chain? = null
    /** The NV12 screen, either way. */
    private val screen: Long get() = yuvFront?.handle ?: yuv?.handle ?: 0L
    /** False: the compositor-paced chain instead of the front buffer. */
    var lowestLatency = true
    private val overlayCursor by lazy { HostCursor(view) }
    /** Fallback: RGBA buffers the compositor shows. */
    private var chain: SwapChain? = null
    /** Experiments only: writing into the buffer on screen (front-buffer rendering). */
    private var front: CpuFront? = null
    private val handle: Long get() = chain?.handle ?: front?.handle ?: 0L

    /** Experiments (`--ei variant` 1-8): the front-buffer variants. */
    var useFrontBuffer = false
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
    /**
     * What video frames that were given up on (never decoded in time) would have changed: the
     * next decoded frame (a complete, newer picture) redraws it too, or it would stay stale on
     * screen (a closed menu still showing).
     */
    private var carried: IntArray? = null
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

    private val ready get() = screen != 0L || handle != 0L

    /** Pixels of what is drawn into: the stream's size for the NV12 paths (scaled to the screen by the compositor). */
    private val paintWidth get() = if (screen != 0L && streamW > 0) streamW else view.width
    private val paintHeight get() = if (screen != 0L && streamH > 0) streamH else view.height

    /**
     * The host's choice: the front buffer (where supported) or the swap chain. A change rebuilds
     * the buffers; the picture comes back with the next frame (a full one after a rebuild).
     */
    fun chooseLowestLatency(on: Boolean) {
        handler.post {
            if (lowestLatency == on) return@post
            lowestLatency = on
            if (yuv != null || yuvFront != null) {
                yuv?.release()
                yuv = null
                yuvFront?.release()
                yuvFront = null
                fullNext = true
                attach()
            }
        }
    }

    /** None of the CPU paths could be set up on this device: the app falls back to plain video. */
    var onUnusable: (() -> Unit)? = null
    private var failedAttaches = 0

    private fun attach() {
        if (ready) return
        // The connection waits for this surface, and the stream's size comes over it: offered
        // as soon as the view has one, not once the buffers exist.
        if (surface == null && view.holder.surface?.isValid == true) surface = view.holder.surface
        // Buffers the stream's size (the compositor scales them to the screen): known once
        // the host has sent its configuration.
        val (bw, bh) = streamW to streamH
        if (bw > 0) when {
            useFrontBuffer -> front = CpuFront.attach(view)
            lowestLatency && YuvFront.supported() -> yuvFront = YuvFront.attach(view, bw, bh)
            lowestLatency && NdkFront.supported() -> yuvFront = NdkFront.attach(view, bw, bh)
            YuvChain.supported() -> yuv = YuvChain.attach(view, handler, bw, bh)
            NdkChain.supported() -> yuv = NdkChain.attach(view, handler, bw, bh)
            SwapChain.supported() -> chain = SwapChain.attach(view, handler)
        }
        if (!ready) {
            // Laid out and still nothing: this device cannot do it (not just "not yet").
            val laidOut = bw > 0 && view.width > 0 && view.holder.surface?.isValid == true
            if (laidOut && ++failedAttaches >= 5) {
                TLog.i("the fast display paths could not be set up here; switching to plain video")
                onUnusable?.invoke()
                return
            }
            handler.postDelayed(::attach, 100) // the view is not laid out yet
            return
        }
        surface = view.holder.surface
        followVsync()
        hint = try {
            view.context.getSystemService(android.os.PerformanceHintManager::class.java)
                ?.createHintSession(intArrayOf(Process.myTid()), 1_000_000L) // 1 ms per update
        } catch (_: Exception) {
            null
        }
    }

    override fun frameDecoded(pts: Long, image: Image, done: () -> Unit) {
        decoded.add(Triple(pts, image, done))
        schedule()
    }

    /** Decoded frames not yet taken over by the render thread (into [frames]). */
    private val decoded = ConcurrentLinkedQueue<Triple<Long, Image, () -> Unit>>()

    /**
     * One [process] pending at a time: a present can wait most of a refresh for the scan, and
     * one per message (frames, tiles, cursor moves) queued them up behind each other, ~100 ms
     * behind at 85 fps. Whatever arrives meanwhile goes into the next one together.
     */
    private val processPending = java.util.concurrent.atomic.AtomicBoolean(false)

    private fun schedule() {
        if (processPending.compareAndSet(false, true)) handler.post {
            processPending.set(false)
            process()
        }
    }

    override fun configure(w: Int, h: Int) {
        handler.post {
            // A new stream size: buffers of the new size.
            if ((w != streamW || h != streamH) && (yuv != null || yuvFront != null)) {
                yuv?.release()
                yuv = null
                yuvFront?.release()
                yuvFront = null
            }
            streamW = w
            streamH = h
            if (!ready) attach()
            fullNext = true
            while (true) {
                val u = updates.poll() ?: break
                if (u is Tile) { Native.recycle(u.luma); Native.recycle(u.chroma) }
            }
            while (true) decoded.poll()?.third?.invoke() ?: break
            frames.values.forEach { it.second() }
            frames.clear()
        }
    }

    override fun queueVideo(pts: Long) {
        updates.add(Video(pts))
    }

    override fun queueTile(pts: Long, x: Int, y: Int, w: Int, h: Int, luma: ByteBuffer, chroma: ByteBuffer) {
        updates.add(Tile(pts, x, y, w, h, luma, chroma))
        schedule()
    }

    override fun setCursorImage(bitmap: Bitmap?, displayWidthPt: Int, sizePt: IntArray, hotPt: IntArray) {
        if (yuv != null && yuvFront == null) return overlayCursor.setImage(bitmap, displayWidthPt, sizePt, hotPt)
        if (bitmap == null || displayWidthPt == 0) return
        val scale = paintWidth.toFloat() / displayWidthPt
        // Scaled to the panel once here; the native sprite is drawn 1:1.
        val w = maxOf(1, (sizePt[0] * scale).toInt())
        val h = maxOf(1, (sizePt[1] * scale).toInt())
        cursorHot = intArrayOf((hotPt[0] * scale).toInt(), (hotPt[1] * scale).toInt())
        cursorScale = scale
        cursorImage = Bitmap.createScaledBitmap(bitmap.copy(Bitmap.Config.ARGB_8888, false), w, h, true)
        scheduleCursor()
    }

    override fun moveCursor(x: Int, y: Int, shown: Boolean) {
        if (yuv != null && yuvFront == null) return overlayCursor.move(x, y, shown)
        cursorX = x; cursorY = y; cursorShown = shown
        scheduleCursor()
    }

    /** One cursor redraw pending at a time (moves come in faster than the scan passes). */
    private val cursorPending = java.util.concurrent.atomic.AtomicBoolean(false)

    private fun scheduleCursor() {
        if (cursorPending.compareAndSet(false, true)) handler.post {
            cursorPending.set(false)
            drawCursor()
        }
    }

    private fun drawCursor() {
        val s = screen
        if (yuvFront != null) {
            val img = cursorImage
            if (img != null && img !== cursorUploaded) {
                val buf = ByteBuffer.allocateDirect(img.byteCount).order(ByteOrder.nativeOrder())
                img.copyPixelsToBuffer(buf) // premultiplied RGBA
                Native.yuvCursorImage(s, buf, img.width, img.height)
                cursorUploaded = img
            }
            val x = cursorX / 65535f * paintWidth - cursorHot[0]
            val y = cursorY / 65535f * paintHeight - cursorHot[1]
            Native.yuvCursorMove(s, x.toInt(), y.toInt(), cursorShown && img != null)
            Native.yuvPresent(s, false)
            return
        }
        val h = handle
        if (h == 0L) return
        val img = cursorImage
        if (img != null && img !== cursorUploaded) {
            val buf = ByteBuffer.allocateDirect(img.byteCount).order(ByteOrder.nativeOrder())
            img.copyPixelsToBuffer(buf) // premultiplied RGBA
            Native.frontCursorImage(h, buf, img.width, img.height)
            cursorUploaded = img
        }
        val x = cursorX / 65535f * view.width - cursorHot[0]
        val y = cursorY / 65535f * view.height - cursorHot[1]
        Native.frontCursorMove(h, x.toInt(), y.toInt(), cursorShown && img != null)
        show()
    }

    /**
     * Follows the panel's vsync, so the native side knows where the scan is: the expected
     * presentation time of a frame timeline is a hardware vsync, when the scan starts over.
     */
    private var vsyncLoop = 0

    private fun followVsync() {
        val choreographer = android.view.Choreographer.getInstance() // the render thread's
        // One loop: an attach after a rebuild ends the one before.
        val loop = ++vsyncLoop
        if (android.os.Build.VERSION.SDK_INT < 33) {
            // Older: the frame time is the app's vsync, close enough to the panel's.
            val cb = object : android.view.Choreographer.FrameCallback {
                override fun doFrame(t: Long) {
                    if (loop != vsyncLoop) return
                    val hz = view.display?.refreshRate ?: 60f
                    yuvFront?.let {
                        // The panel's vsync if known: the frame time is the app's wake-up.
                        val v = it.vsync()
                        Native.yuvVsync(it.handle, if (v > 0) v else t, (1e9 / hz).toLong())
                        if (streamW > 0) it.refresh()
                        choreographer.postFrameCallback(this)
                        return
                    }
                    if (handle == 0L) return
                    Native.frontVsync(handle, t, (1e9 / hz).toLong())
                    choreographer.postFrameCallback(this)
                }
            }
            choreographer.postFrameCallback(cb)
            return
        }
        val callback = object : android.view.Choreographer.VsyncCallback {
            override fun onVsync(data: android.view.Choreographer.FrameData) {
                if (loop != vsyncLoop) return
                val hz = view.display?.refreshRate ?: 60f
                yuvFront?.let {
                    Native.yuvVsync(it.handle, data.preferredFrameTimeline.expectedPresentationTimeNanos, (1e9 / hz).toLong())
                    // Every refresh while streaming: keeps the display at its full rate.
                    if (streamW > 0) it.refresh()
                    choreographer.postVsyncCallback(this)
                    return
                }
                if (handle == 0L) return
                Native.frontVsync(handle, data.preferredFrameTimeline.expectedPresentationTimeNanos, (1e9 / hz).toLong())
                val f = front
                if (f != null && streamW > 0) {
                    if (CpuFront.singleBuffer) f.damaged() else f.flip()
                }
                choreographer.postVsyncCallback(this)
            }
        }
        choreographer.postVsyncCallback(callback)
    }

    /** Applies queued updates in order until one is a video frame still being decoded. */
    private fun process() {
        if (!ready) return
        val started = System.nanoTime()
        try {
            processUpdates()
            show()
        } finally {
            reportWork(started)
        }
    }

    /**
     * Puts everything converted so far on screen at once (a frame's tiles together). With the
     * swap chain: into a free buffer, handed to the compositor; frames count as shown when it
     * reaches the screen. If every buffer is still with the compositor, this runs again as
     * soon as one comes back, with whatever arrived meanwhile.
     */
    private fun show() {
        val f0 = yuvFront
        if (f0 != null) {
            // Into the scanned-out buffer, timed against the scan: on the panel as it passes.
            // Tiles of one Mac frame go out as separate messages, pts 1 µs apart: one frame.
            val newFrame = shown.any { it - lastFramePts > 1000 }
            shown.maxOrNull()?.let { lastFramePts = maxOf(lastFramePts, it) }
            Native.yuvPresent(f0.handle, newFrame)
            if (newFrame) Native.yuvCountFrame(f0.handle)
            val now = System.nanoTime()
            for (p in shown) onShown(p, now)
            shown.clear()
            return
        }
        val y = yuv
        if (y != null) {
            val batch = ArrayList(shown)
            if (y.submit(onShown = { at -> for (p in batch) onShown(p, at) }, retry = ::schedule)) {
                shown.clear()
            }
            return
        }
        val c = chain
        if (c != null) {
            val batch = ArrayList(shown)
            if (c.submit(onShown = { at -> for (p in batch) onShown(p, at) }, retry = ::schedule)) {
                shown.clear()
            }
            return
        }
        val f = front ?: return
        Native.frontPresent(f.handle)
        if (shown.isNotEmpty()) {
            if (CpuFront.singleBuffer) f.damaged()
            Native.frontCountFrame(f.handle)
        }
        val now = System.nanoTime()
        for (p in shown) onShown(p, now)
        shown.clear()
    }

    /** The newest update presented (front mode). */
    private var lastFramePts = Long.MIN_VALUE / 2

    /** Updates converted but not yet presented (render thread only). */
    private val shown = ArrayList<Long>()

    /** Per decoded frame: copying it out, and from its message's arrival to the copy (log). */
    private val copyTimes = ArrayList<Long>()
    private val queueTimes = ArrayList<Long>()

    /** Decoded frames given back unshown (see [processUpdates]); logged when it changes. */
    private var orphans = 0
    /** Video frames given up on (not decoded within 150 ms); counted, logged now and then. */
    private var skipped = 0
    private var skippedLogged = 0L

    private fun processUpdates() {
        val h = handle
        while (true) {
            val (pts, image, done) = decoded.poll() ?: break
            frames.put(pts, image to done)?.second?.invoke()
        }
        while (true) {
            val u = updates.peek() ?: break
            when (u) {
                is Tile -> {
                    val cr = u.chroma.duplicate().also { it.position(1) }.slice()
                    if (screen != 0L) Native.yuvUpdate(screen, u.luma, u.w, u.chroma, cr, u.w, 2, u.x, u.y, u.x, u.y, u.x + u.w, u.y + u.h)
                    else Native.frontYuv(h, u.luma, u.w, u.chroma, cr, u.w, 2, u.x, u.y, u.x, u.y, u.x + u.w, u.y + u.h)
                    Native.recycle(u.luma)
                    Native.recycle(u.chroma)
                    shown.add(u.pts)
                    updates.poll()
                }
                is Video -> {
                    val frame = frames.remove(u.pts)
                    when {
                        frame != null -> {
                            val t0 = System.nanoTime()
                            drawFrame(h, frame.first, u.pts)
                            copyTimes.add(System.nanoTime() - t0)
                            queueTimes.add(t0 - u.queuedAt)
                            if (copyTimes.size >= 400) {
                                copyTimes.sort(); queueTimes.sort()
                                val q = { l: ArrayList<Long>, f: Double -> "%.1f".format(l[((l.size - 1) * f).toInt()] / 1e6) }
                                TLog.i("decoded frames: copy ${q(copyTimes, 0.5)}/${q(copyTimes, 0.95)} ms, queued ${q(queueTimes, 0.5)}/${q(queueTimes, 0.95)} ms (median/p95)")
                                copyTimes.clear(); queueTimes.clear()
                            }
                            frame.second()
                            shown.add(u.pts)
                            updates.poll()
                        }
                        // Lost (the decoder skipped it, or is far behind): decoding takes ~8-40 ms, so
                        // 150 ms is plenty, and what is queued behind it does not wait longer.
                        frames.keys.any { it > u.pts } || System.nanoTime() - u.queuedAt > 150_000_000L -> {
                            // Its area is redrawn from the next decoded frame (see [carried]).
                            val a = changedArea(u.pts)
                            if (a == null) fullNext = true
                            else carried = carried?.let { c ->
                                intArrayOf(minOf(c[0], a[0]), minOf(c[1], a[1]), maxOf(c[2], a[2]), maxOf(c[3], a[3]))
                            } ?: a
                            skipped++
                            updates.poll()
                        }
                        else -> {
                            handler.postDelayed(::schedule, 10) // in case it never comes
                            return
                        }
                    }
                }
            }
        }
        if (skipped + orphans > 0 && System.nanoTime() - skippedLogged > 10_000_000_000L) {
            TLog.i("video frames late: $skipped not decoded within 150 ms (their areas redrawn from the next), $orphans arrived after")
            skipped = 0
            orphans = 0
            skippedLogged = System.nanoTime()
        }
        // Frames nobody waits for (they came after their update was given up as lost): give
        // them back now. Each holds one of the decoder's few output buffers; once all are
        // held it stops taking input and the stream freezes.
        if (frames.isNotEmpty()) {
            val waiting = updates.mapNotNullTo(HashSet()) { (it as? Video)?.pts }
            val it = frames.entries.iterator()
            while (it.hasNext()) {
                val e = it.next()
                if (e.key !in waiting) {
                    e.value.second()
                    it.remove()
                    ++orphans
                }
            }
        }
    }

    private fun drawFrame(target: Long, img: Image, pts: Long) {
        val w = minOf(img.width, if (screen != 0L) streamW else view.width)
        val h = minOf(img.height, if (screen != 0L) streamH else view.height)
        val own = if (fullNext) null else changedArea(pts)
        val extra = carried
        val c = when {
            own == null -> null
            extra == null -> own
            else -> intArrayOf(minOf(own[0], extra[0]), minOf(own[1], extra[1]), maxOf(own[2], extra[2]), maxOf(own[3], extra[3]))
        }
        fullNext = false
        carried = null
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
        if (screen != 0L) {
            Native.yuvUpdate(screen, p[0].buffer, p[0].rowStride, p[1].buffer, p[2].buffer, p[1].rowStride, p[1].pixelStride,
                0, 0, x0, y0, x1, y1)
            return
        }
        Native.frontYuv(target, p[0].buffer, p[0].rowStride, p[1].buffer, p[2].buffer, p[1].rowStride, p[1].pixelStride,
            0, 0, x0, y0, x1, y1)
    }

    /** Saves what the panel shows to [path] as a PNG (debugging). */
    fun dump(path: java.io.File) {
        handler.post {
            val h = handle
            if (screen != 0L) {
                val b = dumpBuf ?: ByteBuffer.allocateDirect(paintWidth * paintHeight * 4).order(ByteOrder.nativeOrder()).also { dumpBuf = it }
                b.clear()
                Native.yuvDump(screen, b)
                val bmp = Bitmap.createBitmap(paintWidth and 1.inv(), paintHeight and 1.inv(), Bitmap.Config.ARGB_8888)
                b.rewind()
                bmp.copyPixelsFromBuffer(b)
                java.io.File(path.parentFile, "shadow.png").outputStream().use { bmp.compress(Bitmap.CompressFormat.PNG, 100, it) }
                return@post
            }
            if (h == 0L) return@post
            val buf = dumpBuf ?: ByteBuffer.allocateDirect(view.width * view.height * 4).order(ByteOrder.nativeOrder()).also { dumpBuf = it }
            fun save(file: java.io.File) {
                val bmp = Bitmap.createBitmap(view.width, view.height, Bitmap.Config.ARGB_8888)
                buf.rewind()
                bmp.copyPixelsFromBuffer(buf)
                file.outputStream().use { bmp.compress(Bitmap.CompressFormat.PNG, 100, it) }
            }
            // What the panel scans out, and what it should show.
            buf.clear()
            if (Native.frontDumpScanout(h, buf)) save(path)
            buf.clear()
            Native.frontDump(h, buf)
            save(java.io.File(path.parentFile, "shadow.png"))
            android.util.Log.i("tabdisplay", "dumped the front buffer to $path")
        }
    }

    override fun release() {
        handler.post {
            while (true) decoded.poll()?.third?.invoke() ?: break
            frames.values.forEach { it.second() }
            front?.release()
            front = null
            chain?.release()
            chain = null
            yuv?.release()
            yuv = null
            yuvFront?.release()
            yuvFront = null
            overlayCursor.release()
        }
        thread.quitSafely()
    }
}
