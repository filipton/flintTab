package dev.tabdisplay

import android.media.AudioAttributes
import android.media.AudioFormat
import android.media.AudioTrack
import android.media.MediaCodec
import android.media.MediaCodecInfo
import android.media.MediaCodecList
import android.media.MediaFormat
import android.os.Build
import android.os.Handler
import android.os.HandlerThread
import android.os.Process
import android.view.Surface
import java.io.DataInputStream
import java.io.DataOutputStream
import java.net.InetSocketAddress
import java.net.Socket
import java.io.Closeable
import java.io.FileInputStream
import java.io.FileOutputStream
import java.io.InputStream
import java.io.OutputStream
import android.hardware.usb.UsbManager
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.LinkedBlockingDeque
import java.util.concurrent.LinkedBlockingQueue
import java.util.concurrent.TimeUnit
import kotlin.concurrent.thread

/**
 * Connects to the Mac (through `adb reverse`, so 127.0.0.1) and plays what it sends.
 * See host/src/protocol.rs for the wire format.
 */
class Session(
    private val port: Int,
    private val screenW: Int,
    private val screenH: Int,
    /** Highest frame rate to ask for; read at every connection (the panel's modes can change). */
    private val maxFps: () -> Int,
    /** The panel's highest refresh rate; the video surface asks for it whatever the stream's rate. */
    private val panelHz: () -> Int,
    private val surfaceProvider: () -> Surface?,
    /** The surface is a GL texture drawn by [FrontRenderer], which reports when frames are shown. */
    private val toTexture: Boolean = false,
    private val onState: (connected: Boolean) -> Unit,
    /** The computer's mouse: x, y as 0..65535 across the display, and whether it is on it. */
    private val onCursor: (x: Int, y: Int, visible: Boolean) -> Unit = { _, _, _ -> },
    /** MSG_CURSOR_IMAGE payload. */
    private val onCursorImage: (ByteArray) -> Unit = {},
    /** Where frames are drawn when [toTexture]: told the stream size, every video frame (before
     *  it is decoded, to keep the order) and every tile. */
    private val front: Renderer? = null,
    /** For the USB accessory link; null: adb's TCP forward only. */
    private val usb: UsbManager? = null,
    /** Asks the user once to let the app use the accessory (when not opened through it). */
    private val askUsbPermission: (android.hardware.usb.UsbAccessory) -> Unit = {},
) {
    @Volatile private var running = true
    @Volatile private var socket: Closeable? = null
    private var askedUsb = false
    @Volatile private var out: DataOutputStream? = null
    @Volatile private var audioWanted = false
    @Volatile private var audio: AudioPlayer? = null

    private val worker = thread(name = "session", isDaemon = true) {
        Process.setThreadPriority(Process.THREAD_PRIORITY_URGENT_DISPLAY)
        loop()
    }

    fun setAudio(enabled: Boolean) {
        audioWanted = enabled
        audio?.setEnabled(enabled)
        sendControl(KIND_AUDIO, if (enabled) 1 else 0)
    }

    /** The rate the host was told; it streams (and sizes its virtual display) for it. */
    @Volatile private var sentFps = 0

    /**
     * The panel's modes changed (the host lifts the 60 Hz caps as the app starts, often after
     * it said hello): reconnect if the stream rate should change, or a 60 fps stream lands on a
     * 90 Hz panel and moves unevenly (one frame every 1.5 refreshes).
     */
    fun panelChanged() {
        val fps = maxFps()
        if (sentFps == 0 || fps == sentFps) return
        android.util.Log.i("tabdisplay", "panel rate changed ($sentFps -> $fps fps): reconnecting")
        try { socket?.close() } catch (_: Exception) {}
    }

    fun stop() {
        running = false
        try { socket?.close() } catch (_: Exception) {}
        worker.join(1500)
    }

    /** Control messages go out on their own thread: touch events arrive on the UI thread,
     *  where Android forbids network I/O. */
    private val outbox = LinkedBlockingQueue<ByteArray>()
    private val sender = thread(name = "control-out", isDaemon = true) {
        Process.setThreadPriority(Process.THREAD_PRIORITY_URGENT_DISPLAY)
        while (running) {
            val msg = outbox.poll(200, TimeUnit.MILLISECONDS) ?: continue
            val o = out ?: continue
            try {
                synchronized(o) { o.write(msg); o.flush() }
            } catch (_: Exception) {}
        }
    }

    private val ackMsg = byteArrayOf(KIND_ACK.toByte(), 0)

    /** Right from the reading thread: the host holds back frames until the ack is in. */
    private fun ack(output: DataOutputStream) {
        synchronized(output) { output.write(ackMsg); output.flush() }
    }

    private fun sendLog(line: String) {
        val text = line.toByteArray(Charsets.UTF_8).let { if (it.size > 2000) it.copyOf(2000) else it }
        outbox.add(byteArrayOf(KIND_LOG.toByte(), 0, (text.size shr 8).toByte(), text.size.toByte()) + text)
    }

    private fun sendControl(kind: Int, value: Int) {
        if (out != null) outbox.add(byteArrayOf(kind.toByte(), value.toByte()))
    }

    /** Pointer event at a position given as 0..65535 across the stream. */
    fun sendPointer(action: Int, x: Int, y: Int) {
        if (out == null) return
        outbox.add(byteArrayOf(Proto.KIND_POINTER.toByte(), action.toByte(),
            (x shr 8).toByte(), x.toByte(), (y shr 8).toByte(), y.toByte()))
    }

    /**
     * Per-frame timestamps (µs, this device's clock) for the host's latency breakdown:
     * recv_start, recv_end, queued, decoded. Keyed by the host's pts.
     */
    private val frameTimes = ConcurrentHashMap<Long, LongArray>()

    private fun nowUs() = System.nanoTime() / 1000

    /** Changed area of each frame in flight, by pts: x0, y0, x1, y1 as 0..65535. */
    private val changedAreas = ConcurrentHashMap<Long, IntArray>()

    /** The area frame [pts] changed (and forgets it); null if unknown. */
    fun takeChangedArea(pts: Long): IntArray? = changedAreas.remove(pts)

    private fun onDecoded(pts: Long) {
        frameTimes[pts]?.set(3, nowUs())
    }

    /** The frame reached the panel: send all its timestamps to the host. */
    fun frameShown(pts: Long, nanos: Long) {
        val t = frameTimes.remove(pts) ?: return
        frameTimes.keys.removeIf { it < pts } // skipped frames never render
        if (out == null || t[3] == 0L) return
        val b = ByteArray(2 + 8 * 6)
        b[0] = KIND_TIMING.toByte()
        putLong(b, 2, pts)
        for (i in 0 until 4) putLong(b, 10 + 8 * i, t[i])
        putLong(b, 42, nanos / 1000)
        outbox.add(b)
    }

    private fun putLong(b: ByteArray, at: Int, v: Long) {
        for (i in 0 until 8) b[at + i] = (v shr (56 - 8 * i)).toByte()
    }

    /** Two-finger scroll, finger movement in pixels. */
    fun sendScroll(dx: Int, dy: Int) {
        if (out == null) return
        val x = dx.coerceIn(-32768, 32767); val y = dy.coerceIn(-32768, 32767)
        outbox.add(byteArrayOf(Proto.KIND_SCROLL.toByte(), 0,
            (x shr 8).toByte(), x.toByte(), (y shr 8).toByte(), y.toByte()))
    }

    private fun loop() {
        while (running) {
            val surface = surfaceProvider()
            if (surface == null || !surface.isValid) { Thread.sleep(100); continue }
            try {
                runOnce(surface)
            } catch (e: Exception) {
                // Connection refused (no host yet): retry below quietly. Anything once connected
                // ended the session: say what.
                if (connectedOnce) TLog.i("session ended on the tablet: $e at ${e.stackTrace.take(4).joinToString(" < ")}")
            }
            onState(false)
            if (running) Thread.sleep(500)
        }
    }

    /** A link to the host: [Closeable] ends it and unblocks reads. */
    private class Link(val input: InputStream, val output: OutputStream, val close: Closeable, val via: String)

    /**
     * The USB accessory when the host switched the tablet to one (raw bulk transfers, ~0.3 ms
     * round trips instead of adb's ~4 ms), else adb's forward to 127.0.0.1.
     */
    private fun connect(): Link {
        val acc = usb?.accessoryList?.firstOrNull { it.manufacturer == "tabdisplay" }
        if (acc != null) {
            if (usb.hasPermission(acc)) {
                val fd = usb.openAccessory(acc)
                if (fd != null) {
                    // The accessory driver hands out at most 16 KB per read.
                    return Link(AccessoryInput(FileInputStream(fd.fileDescriptor)), FileOutputStream(fd.fileDescriptor), fd, "USB accessory")
                }
            } else if (!askedUsb) {
                askedUsb = true
                askUsbPermission(acc)
            }
        }
        val s = Socket()
        s.tcpNoDelay = true
        s.connect(InetSocketAddress("127.0.0.1", port), 1000)
        return Link(s.getInputStream().buffered(1 shl 16), s.getOutputStream(), s, "adb")
    }

    /**
     * How careful the decoder setup is: 0 the hardware decoder with every low-latency option,
     * 1 the hardware decoder plain, 2 Android's software decoder. A level that does not work on
     * this device (a decoder failing, or taking frames and giving none back) moves to the next.
     */
    @Volatile private var connectedOnce = false
    var decoderLevel = 0
    private val decoderFailures = ArrayList<Long>()

    private fun newDecoder(surface: Surface, w: Int, h: Int, fps: Int): Decoder {
        val r = front
        while (true) {
            try {
                return if (r != null && r.wantsImages) {
                    Decoder(null, w, h, fps, toTexture, decoderLevel, ::onDecoded, ::frameShown) { pts, img, done -> r.frameDecoded(pts, img, done) }
                } else {
                    Decoder(surface, w, h, fps, toTexture, decoderLevel, ::onDecoded, ::frameShown)
                }
            } catch (e: Exception) {
                if (decoderLevel >= 2) throw e
                TLog.i("decoder setup ${decoderLevel} failed (${e.message}); trying ${decoderLevel + 1}")
                decoderLevel++
            }
        }
    }


    private fun runOnce(surface: Surface) {
        val link = connect()
        val s = link.close
        socket = s
        android.util.Log.i("tabdisplay", "connecting over ${link.via}")
        val input = DataInputStream(link.input)
        val output = DataOutputStream(link.output)

        // handshake: magic, version, screen size, refresh rate, features. One write, so over USB
        // it is one transfer: the host resynchronises on a transfer that starts with the magic.
        val hello = java.io.ByteArrayOutputStream().also {
            DataOutputStream(it).apply {
                write("TDSP".toByteArray())
                writeByte(VERSION)
                writeInt(screenW)
                writeInt(screenH)
                writeInt(maxFps().also { sentFps = it })
                writeByte(if (front != null) FEATURE_TILES else 0)
            }
        }.toByteArray()
        synchronized(output) {
            output.write(hello)
            output.flush()
        }
        outbox.clear() // nothing from an earlier connection
        out = output
        TLog.connected(::sendLog)
        connectedOnce = true

        var decoder: Decoder? = null
        var config: IntArray? = null // w, h, fps
        var needKeyframe = false
        var frameBuf = ByteArray(1 shl 20) // reused: no per-frame allocation/GC
        /**
         * The decoder misbehaved: rebuilt, and a keyframe for it. One that gave nothing back at
         * all, or failed three times within half a minute, makes way for the next setup; a
         * single hiccup does not (a slow frame is not a broken decoder).
         */
        fun replaceDecoder(d: Decoder, why: String, broken: Boolean) {
            d.close()
            val now = System.nanoTime()
            decoderFailures.removeAll { now - it > 30_000_000_000L }
            decoderFailures.add(now)
            if ((broken || decoderFailures.size >= 3) && decoderLevel < 2) {
                decoderLevel++
                decoderFailures.clear()
                TLog.i("decoder ${d.name}: $why; switching to setup $decoderLevel")
            } else {
                TLog.i("decoder ${d.name}: $why; restarting it")
            }
            val c = config!!
            decoder = newDecoder(surface, c[0], c[1], c[2])
            needKeyframe = true
            sendControl(KIND_IDR, 0)
        }
        // The host answers the handshake at once and pings every second: silence means the
        // link is dead (over USB the hello can get lost when the app restarts), so start over.
        val lastRx = java.util.concurrent.atomic.AtomicLong(System.nanoTime())
        val watchdog = thread(name = "link-watchdog", isDaemon = true) {
            try {
                while (true) {
                    Thread.sleep(500)
                    if (System.nanoTime() - lastRx.get() > 3_000_000_000L) {
                        android.util.Log.i("tabdisplay", "host silent for 3 s: reconnecting")
                        try { s.close() } catch (_: Exception) {}
                        return@thread
                    }
                }
            } catch (_: InterruptedException) {}
        }
        try {
            while (running) {
                val kind = input.readUnsignedByte()
                lastRx.set(System.nanoTime())
                val arrived = nowUs()
                val len = input.readInt()
                when (kind) {
                    MSG_CONFIG -> {
                        val w = input.readInt(); val h = input.readInt(); val fps = input.readInt()
                        val rate = input.readInt(); val ch = input.readUnsignedByte()
                        decoder?.close()
                        audio?.close()
                        config = intArrayOf(w, h, fps)
                        // Let the display switch to a refresh rate that fits the stream.
                        setStreamFrameRate(surface, maxOf(fps, panelHz()))
                        front?.configure(w, h)
                        decoder = newDecoder(surface, w, h, fps)
                        needKeyframe = false
                        audio = AudioPlayer(rate, ch).also { it.setEnabled(audioWanted) }
                        // host starts with audio off; tell it what the switch currently says
                        sendControl(KIND_AUDIO, if (audioWanted) 1 else 0)
                        onState(true)
                    }
                    MSG_VIDEO -> {
                        val pts = input.readLong() // host clock; echoed back once shown, for latency stats
                        // What changed since the previous frame (0..65535 across it): all the
                        // front renderer has to redraw.
                        val changed = IntArray(4) { input.readUnsignedShort() }
                        val n = len - 16
                        if (changedAreas.size > 256) changedAreas.clear()
                        changedAreas[pts] = changed
                        if (n > frameBuf.size) frameBuf = ByteArray(n * 2)
                        input.readFully(frameBuf, 0, n)
                        val received = nowUs()
                        if (frameTimes.size > 256) frameTimes.clear() // no render callbacks on this device
                        frameTimes[pts] = longArrayOf(arrived, received, 0, 0)
                        if (needKeyframe && isKeyframe(frameBuf, n)) needKeyframe = false
                        val d = decoder
                        if (d != null && !needKeyframe) {
                            front?.queueVideo(pts)
                            val ok = !d.failed && try { d.feed(frameBuf, n, pts) } catch (_: Exception) { false }
                            frameTimes[pts]?.set(2, nowUs())
                            when {
                                // Decoder died (e.g. a codec error): rebuild it and ask the host for
                                // a keyframe instead of tearing the whole connection down.
                                !ok -> replaceDecoder(d, d.error ?: "stopped taking frames", broken = false)
                                d.silent() -> replaceDecoder(d, "took ${d.inputs} frames and gave none back", broken = true)
                            }
                        }
                        // Flow control: the host keeps at most a couple of frames unacknowledged.
                        ack(output)
                    }
                    MSG_AUDIO -> {
                        val data = ByteArray(len)
                        input.readFully(data)
                        audio?.enqueue(data)
                    }
                    MSG_CURSOR -> {
                        val x = input.readUnsignedShort(); val y = input.readUnsignedShort()
                        val visible = input.readUnsignedByte() != 0
                        input.skipBytes(len - 5)
                        onCursor(x, y, visible)
                    }
                    MSG_TILE -> {
                        val pts = input.readLong()
                        val x = input.readUnsignedShort(); val y = input.readUnsignedShort()
                        val w = input.readUnsignedShort(); val h = input.readUnsignedShort()
                        val lumaLen = input.readInt()
                        val n = len - 20
                        if (n > frameBuf.size) frameBuf = ByteArray(n * 2)
                        input.readFully(frameBuf, 0, n)
                        val received = nowUs()
                        val luma = Native.buffer(w * h)
                        val chroma = Native.buffer(w * h / 2)
                        Native.lz4Into(frameBuf, 0, lumaLen, luma)
                        Native.lz4Into(frameBuf, lumaLen, n - lumaLen, chroma)
                        val unpacked = nowUs()
                        frameTimes[pts] = longArrayOf(arrived, received, unpacked, unpacked)
                        front?.queueTile(pts, x, y, w, h, luma, chroma)
                        ack(output)
                    }
                    MSG_PING -> {
                        // Answered right here, not through the outbox, so the round trip stays short.
                        val b = ByteArray(18)
                        b[0] = KIND_PONG.toByte()
                        putLong(b, 2, input.readLong())
                        putLong(b, 10, arrived)
                        input.skipBytes(len - 8)
                        synchronized(output) { output.write(b); output.flush() }
                    }
                    MSG_CURSOR_IMAGE -> {
                        val data = ByteArray(len)
                        input.readFully(data)
                        onCursorImage(data)
                    }
                    else -> input.skipBytes(len)
                }
            }
        } finally {
            TLog.disconnected()
            watchdog.interrupt()
            onCursor(0, 0, false)
            decoder?.close()
            audio?.close()
            audio = null
            out = null
            try { s.close() } catch (_: Exception) {}
        }
    }

    companion object {
        const val VERSION = 4
        const val MSG_CONFIG = 1
        const val MSG_VIDEO = 2
        const val MSG_AUDIO = 3
        const val MSG_CURSOR = 4
        const val MSG_CURSOR_IMAGE = 5
        const val MSG_PING = 6
        const val MSG_TILE = 7
        const val FEATURE_TILES = 1
        const val KIND_AUDIO = 1
        const val KIND_ACK = 2
        const val KIND_IDR = 3
        const val KIND_TIMING = 6
        const val KIND_PONG = 7
        /** u16 length, then that much UTF-8: a log line the host prints. */
        const val KIND_LOG = 8
    }
}

/** Tells the display which rate the stream runs at, so it can switch to a matching refresh rate. */
private fun setStreamFrameRate(surface: Surface, fps: Int) {
    try {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            surface.setFrameRate(fps.toFloat(), Surface.FRAME_RATE_COMPATIBILITY_FIXED_SOURCE,
                Surface.CHANGE_FRAME_RATE_ALWAYS)
        } else {
            surface.setFrameRate(fps.toFloat(), Surface.FRAME_RATE_COMPATIBILITY_FIXED_SOURCE)
        }
    } catch (_: Exception) {}
}

/** True if the Annex-B access unit holds an IDR slice or an SPS (a point a decoder can start at). */
private fun isKeyframe(au: ByteArray, size: Int): Boolean {
    var i = 0
    while (i + 3 < size) {
        if (au[i].toInt() == 0 && au[i + 1].toInt() == 0 && au[i + 2].toInt() == 1) {
            val type = au[i + 3].toInt() and 0x1f
            if (type == 5 || type == 7) return true
            i += 3
        } else {
            i++
        }
    }
    return false
}

/**
 * Highest frame rate up to [refreshHz] the H.264 decoder can handle at this size,
 * so the host never sends more frames than the tablet can show or decode.
 */
fun maxDecodableFps(width: Int, height: Int, refreshHz: Int): Int {
    val caps = MediaCodecList(MediaCodecList.REGULAR_CODECS).codecInfos
        .filter { !it.isEncoder && it.supportedTypes.contains(MediaFormat.MIMETYPE_VIDEO_AVC) }
        .map { it.getCapabilitiesForType(MediaFormat.MIMETYPE_VIDEO_AVC).videoCapabilities }
    // Vendors list performance points only for standard sizes (e.g. 1080p@240, 4K@60), so a
    // tablet size at 90/120 Hz often matches none even when the decoder has plenty of headroom.
    // Also accept the same macroblock rate at 1080p.
    val blocks = ((width + 15) / 16) * ((height + 15) / 16)
    val fhdBlocks = 120 * 68
    for (fps in listOf(refreshHz, 120, 90, 60).filter { it <= refreshHz }.distinct()) {
        val fhdFps = Math.ceil(fps.toDouble() * blocks / fhdBlocks)
        if (caps.any {
                it.areSizeAndRateSupported(width, height, fps.toDouble()) ||
                    (it.isSizeSupported(width, height) && it.areSizeAndRateSupported(1920, 1080, fhdFps))
            }) return fps
    }
    return 60
}

private fun pickDecoder(): MediaCodecInfo? {
    val avc = MediaCodecList(MediaCodecList.REGULAR_CODECS).codecInfos.filter {
        !it.isEncoder && !it.isSoftwareOnly && it.supportedTypes.contains(MediaFormat.MIMETYPE_VIDEO_AVC)
    }
    // Prefer a hardware decoder that advertises low-latency support.
    return avc.firstOrNull {
        it.getCapabilitiesForType(MediaFormat.MIMETYPE_VIDEO_AVC)
            .isFeatureSupported(MediaCodecInfo.CodecCapabilities.FEATURE_LowLatency)
    } ?: avc.firstOrNull()
}

/**
 * Low-latency MediaFormat options, most aggressive first; the first set the codec accepts is
 * used. Keys and fallbacks follow Moonlight's MediaCodecHelper.setDecoderLowLatencyOptions.
 */
/** Android's own software H.264 decoder: slower, but works wherever the hardware one does not. */
private fun pickSoftwareDecoder(): MediaCodecInfo? =
    MediaCodecList(MediaCodecList.REGULAR_CODECS).codecInfos.firstOrNull {
        !it.isEncoder && it.isSoftwareOnly && it.supportedTypes.contains(MediaFormat.MIMETYPE_VIDEO_AVC)
    }

private fun lowLatencyOptions(info: MediaCodecInfo?, fps: Int): List<Map<String, Int>> {
    val name = info?.name?.lowercase() ?: ""
    val qcom = name.startsWith("omx.qcom") || name.startsWith("c2.qti")
    val vendor = when {
        qcom -> mapOf(
            "vendor.qti-ext-dec-picture-order.enable" to 1,
            "vendor.qti-ext-dec-low-latency.enable" to 1,
        )
        name.contains("exynos") -> mapOf("vendor.rtc-ext-dec-low-latency.enable" to 1)
        name.contains("hisi") -> mapOf(
            "vendor.hisi-ext-low-latency-video-dec.video-scene-for-low-latency-req" to 1,
            "vendor.hisi-ext-low-latency-video-dec.video-scene-for-low-latency-rdy" to -1,
        )
        name.contains("amlogic") -> mapOf("vendor.low-latency.enable" to 1)
        else -> emptyMap()
    }
    // MediaTek and Amlogic read this from their modified ACodec.
    val mtk = if (name.contains("mtk") || name.contains("amlogic")) mapOf("vdec-lowlatency" to 1) else emptyMap()
    // Real-time priority, and full clocks: without an operating rate some decoders scale their
    // clock for ordinary playback and take a large part of a frame interval per frame.
    val rt = mapOf(MediaFormat.KEY_LOW_LATENCY to 1, MediaFormat.KEY_PRIORITY to 0)
    val rates = listOf(Short.MAX_VALUE.toInt(), 1000, 480, 240, fps * 2, fps)
    val sets = rates.map { rt + mapOf(MediaFormat.KEY_OPERATING_RATE to it) + vendor + mtk } +
        listOf(rt + vendor + mtk, rt + vendor, rt, emptyMap())
    return sets.distinct()
}

/**
 * H.264 decoder configured for minimum latency, rendering straight to the surface.
 * Runs in async mode: the codec hands over each input and output buffer the moment it is
 * free, so no thread sits in a polling dequeue between a frame's arrival and its display.
 */
private class Decoder(
    surface: Surface?,
    width: Int,
    height: Int,
    fps: Int,
    private val toTexture: Boolean,
    /** Setup level (see Session.newDecoder): 0 low-latency hardware, 1 plain hardware, 2 software. */
    level: Int,
    private val onDecoded: (pts: Long) -> Unit,
    private val onRendered: (pts: Long, nanos: Long) -> Unit,
    /** Without a surface: each decoded frame as an Image, and how to give its buffer back. */
    private val onImage: ((pts: Long, image: android.media.Image, done: () -> Unit) -> Unit)? = null,
) {
    private val codec: MediaCodec
    @Volatile private var open = true
    /** Set when the codec reported an error; the owner then rebuilds it. */
    @Volatile var failed = false
        private set
    private val freeInputs = LinkedBlockingQueue<Int>()
    /** How long without a free input means stuck: software decoding is just slow. */
    private val stuckNanos = if (level >= 2) 1_000_000_000L else 500_000_000L
    val name: String
    /** Frames fed and frames out, for the watchdog: a decoder can take everything and give nothing. */
    @Volatile var inputs = 0
        private set
    @Volatile var outputs = 0
        private set
    private var firstInputAt = 0L
    /** What the codec reported when it failed. */
    @Volatile var error: String? = null
        private set
    private val callbacks = HandlerThread("decoder", Process.THREAD_PRIORITY_URGENT_DISPLAY).apply { start() }

    init {
        val info = if (level >= 2) pickSoftwareDecoder() else pickDecoder()
        codec = if (info != null) MediaCodec.createByCodecName(info.name)
        else MediaCodec.createDecoderByType(MediaFormat.MIMETYPE_VIDEO_AVC)
        val handler = Handler(callbacks.looper)
        val callback = object : MediaCodec.Callback() {
            override fun onInputBufferAvailable(c: MediaCodec, index: Int) {
                freeInputs.add(index)
            }

            override fun onOutputBufferAvailable(c: MediaCodec, index: Int, bi: MediaCodec.BufferInfo) {
                if (!open) return
                val pts = bi.presentationTimeUs
                outputs++
                onDecoded(pts)
                val sink = onImage
                if (sink != null) {
                    // CPU path: the codec's own linear YUV image (a surface would get the
                    // decoder's tiled/compressed layout, which only the GPU can read).
                    val img = try { c.getOutputImage(index) } catch (_: Exception) { null }
                    if (img == null) {
                        try { c.releaseOutputBuffer(index, false) } catch (_: Exception) {}
                        return
                    }
                    sink(pts, img) {
                        try { img.close(); c.releaseOutputBuffer(index, false) } catch (_: Exception) {}
                    }
                    return
                }
                try {
                    // Show at the next vsync; if a newer frame targets the same vsync, the
                    // compositor drops this one (Moonlight's min-latency mode).
                    if (toTexture) c.releaseOutputBuffer(index, true)
                    else c.releaseOutputBuffer(index, System.nanoTime())
                } catch (_: Exception) {
                    failed = true
                }
            }

            override fun onError(c: MediaCodec, e: MediaCodec.CodecException) {
                error = "codec error ${e.errorCode} (${e.diagnosticInfo})"
                failed = true
            }

            override fun onOutputFormatChanged(c: MediaCodec, format: MediaFormat) {}
        }
        var started = false
        // Some decoders take any option in configure() and only refuse it in start() (Exynos
        // cannot reserve real-time resources for too high an operating rate), so try both.
        for (opts in if (level == 0) lowLatencyOptions(info, fps) else listOf(emptyMap())) {
            val fmt = MediaFormat.createVideoFormat(MediaFormat.MIMETYPE_VIDEO_AVC, width, height)
            for ((k, v) in opts) fmt.setInteger(k, v)
            try {
                codec.setCallback(callback, handler)
                codec.configure(fmt, surface, null, 0)
                // When each frame reached the screen (the front renderer reports that itself).
                if (!toTexture) codec.setOnFrameRenderedListener({ _, pts, nanos -> onRendered(pts, nanos) }, handler)
                codec.start()
                started = true
                val sized = try {
                    codec.codecInfo.getCapabilitiesForType(MediaFormat.MIMETYPE_VIDEO_AVC).videoCapabilities.isSizeSupported(width, height)
                } catch (_: Exception) { true }
                TLog.i("decoder ${codec.name} (setup $level) started for ${width}x$height${if (sized) "" else " (it says it does not support that size)"} with $opts")
                break
            } catch (_: Exception) {
                codec.reset()
                freeInputs.clear()
            }
        }
        if (!started) {
            codec.release()
            callbacks.quitSafely()
            throw IllegalStateException("${codec.name} took no configuration")
        }
        name = codec.name
    }

    /** It has taken frames for a while and given none back (some decoders hang that way). */
    fun silent(): Boolean =
        outputs == 0 && inputs >= 30 && System.nanoTime() - firstInputAt > 2_000_000_000L

    /** [pts] is the host's timestamp; it only travels through the codec for latency stats. */
    /**
     * False if the decoder took no input for 50 ms (4+ frames; its outputs are all held, or it hung):
     * the caller rebuilds it rather than blocking the connection's reading thread.
     */
    fun feed(au: ByteArray, size: Int, pts: Long): Boolean {
        val deadline = System.nanoTime() + stuckNanos
        while (open && !failed && System.nanoTime() < deadline) {
            val i = freeInputs.poll(10, TimeUnit.MILLISECONDS) ?: continue
            val buf = codec.getInputBuffer(i)!!
            buf.clear()
            buf.put(au, 0, size)
            codec.queueInputBuffer(i, 0, size, pts, 0)
            if (inputs++ == 0) firstInputAt = System.nanoTime()
            return true
        }
        if (open && !failed) error = "took no input for ${stuckNanos / 1_000_000} ms"
        return false
    }

    fun close() {
        open = false
        try { codec.stop() } catch (_: Exception) {}
        codec.release()
        callbacks.quitSafely()
    }
}

/** Low-latency PCM playback with a tiny jitter buffer; old audio is dropped, never queued up. */
private class AudioPlayer(rate: Int, channels: Int) {
    private val track: AudioTrack
    private val queue = LinkedBlockingDeque<ByteArray>()
    private val maxQueuedBytes = rate * channels * 2 * 60 / 1000 // 60 ms
    private var queuedBytes = 0
    @Volatile private var enabled = false
    @Volatile private var open = true
    private val worker: Thread

    init {
        val chMask = if (channels == 1) AudioFormat.CHANNEL_OUT_MONO else AudioFormat.CHANNEL_OUT_STEREO
        val fmt = AudioFormat.Builder()
            .setEncoding(AudioFormat.ENCODING_PCM_16BIT)
            .setSampleRate(rate)
            .setChannelMask(chMask)
            .build()
        val minBuf = AudioTrack.getMinBufferSize(rate, chMask, AudioFormat.ENCODING_PCM_16BIT)
        track = AudioTrack.Builder()
            .setAudioAttributes(
                AudioAttributes.Builder()
                    .setUsage(AudioAttributes.USAGE_MEDIA)
                    .setContentType(AudioAttributes.CONTENT_TYPE_MUSIC)
                    .build()
            )
            .setAudioFormat(fmt)
            .setBufferSizeInBytes(minBuf)
            .setTransferMode(AudioTrack.MODE_STREAM)
            .setPerformanceMode(AudioTrack.PERFORMANCE_MODE_LOW_LATENCY)
            .build()

        worker = thread(name = "audio-out", isDaemon = true) {
            Process.setThreadPriority(Process.THREAD_PRIORITY_URGENT_AUDIO)
            while (open) {
                val pcm = queue.poll(50, TimeUnit.MILLISECONDS) ?: continue
                synchronized(this) { queuedBytes -= pcm.size }
                if (enabled) track.write(pcm, 0, pcm.size) // blocks at the device's pace
            }
        }
    }

    fun setEnabled(on: Boolean) {
        enabled = on
        if (on) {
            track.play()
        } else {
            track.pause()
            track.flush()
            queue.clear()
            synchronized(this) { queuedBytes = 0 }
        }
    }

    fun enqueue(pcm: ByteArray) {
        if (!enabled) return
        synchronized(this) {
            queue.add(pcm)
            queuedBytes += pcm.size
            while (queuedBytes > maxQueuedBytes && queue.size > 1) {
                queuedBytes -= queue.poll()?.size ?: 0
            }
        }
    }

    fun close() {
        open = false
        worker.join(500)
        try { track.stop() } catch (_: Exception) {}
        track.release()
    }
}

/**
 * Buffered reading from the USB accessory. Not BufferedInputStream: it asks available() how
 * much is waiting, and some accessory drivers (Huawei's) reject that ioctl, which ended every
 * session. Each read asks the driver for at most one 16 KB transfer, which all of them take.
 */
private class AccessoryInput(private val src: java.io.InputStream) : java.io.InputStream() {
    private val buf = ByteArray(1 shl 14)
    private var pos = 0
    private var len = 0

    private fun fill(): Boolean {
        pos = 0
        len = src.read(buf, 0, buf.size)
        return len > 0
    }

    override fun read(): Int {
        if (pos >= len && !fill()) return -1
        return buf[pos++].toInt() and 0xff
    }

    override fun read(b: ByteArray, off: Int, n: Int): Int {
        if (n == 0) return 0
        if (pos >= len) {
            // Large reads straight into the caller's array, one transfer at a time.
            if (n >= buf.size) return src.read(b, off, buf.size)
            if (!fill()) return -1
        }
        val k = minOf(n, len - pos)
        System.arraycopy(buf, pos, b, off, k)
        pos += k
        return k
    }

    override fun close() = src.close()
}
