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
    /** What the host decided (MSG_SETTINGS flags: SETTING_*), once per connection. */
    private val onSettings: (flags: Int) -> Unit = {},
) {
    @Volatile private var running = true
    @Volatile private var socket: Closeable? = null
    private var askedUsb = false
    @Volatile private var out: DataOutputStream? = null
    @Volatile private var audioWanted = false
    @Volatile private var audio: AudioPlayer? = null

    private val worker = thread(name = "session", isDaemon = true) {
        Process.setThreadPriority(Process.THREAD_PRIORITY_URGENT_DISPLAY)
        val tid = Process.myTid()
        HotThreads.add(tid)
        try { loop() } finally { HotThreads.remove(tid) }
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
        val text = line.toByteArray(Charsets.UTF_8).let { if (it.size > 16000) it.copyOf(16000) else it }
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

    /** The part of the screen each frame in flight is (x, y, w, h pixels), by pts. */
    private val regions = ConcurrentHashMap<Long, IntArray>()

    /** The part of the screen frame [pts] is (and forgets it); null: the whole screen. */
    fun takeRegion(pts: Long): IntArray? = regions.remove(pts)

    @Volatile private var keyframeAskedAt = 0L

    /** A whole-screen keyframe from the host (the renderer needs all of it); at most every 200 ms. */
    fun requestKeyframe(minIntervalNanos: Long = 200_000_000L) {
        val now = System.nanoTime()
        if (now - keyframeAskedAt < minIntervalNanos) return
        keyframeAskedAt = now
        sendControl(KIND_IDR, 0)
    }

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
                // (EOF: the host closed it, e.g. moving from adb to raw USB. Not worth a line.)
                if (connectedOnce && e !is java.io.EOFException) TLog.i("session ended on the tablet: $e at ${e.stackTrace.take(4).joinToString(" < ")}")
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
        CrashLog.sendPending()
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
                        onState(true)
                    }
                    MSG_SETTINGS -> {
                        val flags = input.readUnsignedByte()
                        if (len > 1) input.skipBytes(len - 1)
                        audioWanted = flags and SETTING_AUDIO != 0
                        audio?.setEnabled(audioWanted)
                        TLog.i("settings from the host: lowest latency ${flags and SETTING_LOWEST_LATENCY != 0}, " +
                            "touch ${flags and SETTING_TOUCH != 0}, audio $audioWanted")
                        onSettings(flags)
                    }
                    MSG_VIDEO -> {
                        val pts = input.readLong() // host clock; echoed back once shown, for latency stats
                        // What changed since the previous frame (0..65535 across it): all the
                        // front renderer has to redraw.
                        val changed = IntArray(4) { input.readUnsignedShort() }
                        // The part of the screen this picture is (a video's window, or all of it).
                        val region = IntArray(4) { input.readUnsignedShort() }
                        val n = len - 24
                        if (changedAreas.size > 256) changedAreas.clear()
                        changedAreas[pts] = changed
                        if (regions.size > 256) regions.clear()
                        regions[pts] = region
                        if (n > frameBuf.size) frameBuf = ByteArray(n * 2)
                        input.readFully(frameBuf, 0, n)
                        val received = nowUs()
                        if (frameTimes.size > 256) frameTimes.clear() // no render callbacks on this device
                        frameTimes[pts] = longArrayOf(arrived, received, 0, 0)
                        if (needKeyframe && isKeyframe(frameBuf, n)) needKeyframe = false
                        var d = decoder
                        // A part of another size comes with a keyframe: a decoder that cannot change
                        // size on the fly (no adaptive playback) is made anew for it.
                        if (d != null && !d.adaptive && (region[2] != d.width || region[3] != d.height) &&
                            isKeyframe(frameBuf, n)) {
                            d.close()
                            d = newDecoder(surface, region[2], region[3], config!![2])
                            decoder = d
                            needKeyframe = false
                        }
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
        const val VERSION = 6
        const val MSG_CONFIG = 1
        const val MSG_VIDEO = 2
        const val MSG_AUDIO = 3
        const val MSG_CURSOR = 4
        const val MSG_CURSOR_IMAGE = 5
        const val MSG_PING = 6
        const val MSG_TILE = 7
        /** u8 flags: what the host decided (SETTING_*). */
        const val MSG_SETTINGS = 8
        const val SETTING_LOWEST_LATENCY = 1
        const val SETTING_TOUCH = 2
        const val SETTING_AUDIO = 4
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

/**
 * The NDK decoder (android/native/src/codec.rs) instead of the Java one: ~2 ms less per video
 * frame on the Galaxy Tab S10 FE (no Java thread hop or Image per frame). Off once it met an
 * output layout it cannot read, and when the file Android/data/dev.tabdisplay/files/java-decoder
 * exists (comparisons).
 */
private fun nativeDecoderWanted(): Boolean = !nativeDecoderBroken &&
    !java.io.File("/storage/emulated/0/Android/data/dev.tabdisplay/files/java-decoder").exists()

/** The NDK decoder met a picture layout it cannot read: the Java decoder from then on. */
@Volatile private var nativeDecoderBroken = false

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
    val width: Int,
    val height: Int,
    fps: Int,
    private val toTexture: Boolean,
    /** Setup level (see Session.newDecoder): 0 low-latency hardware, 1 plain hardware, 2 software. */
    level: Int,
    private val onDecoded: (pts: Long) -> Unit,
    private val onRendered: (pts: Long, nanos: Long) -> Unit,
    /** Without a surface: each decoded frame as an Image, and how to give its buffer back. */
    private val onImage: ((pts: Long, picture: Picture, done: () -> Unit) -> Unit)? = null,
) {
    private lateinit var codec: MediaCodec
    /**
     * The NDK decoder (android/native/src/codec.rs) in place of [codec], for the CPU path:
     * frames in and out without Java in between (0: the Java MediaCodec is used).
     */
    private var native = 0L
    /** Its pictures still held by the renderer, and whether it was closed: freed at 0 and closed. */
    private var held = 0
    private var nativeClosed = false
    /** Takes pictures of other sizes (up to the configured one) without being made anew. */
    var adaptive = false
        private set
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
    /** Per input: waiting for a free buffer, and handing it over (ns; logged now and then). */
    private val feedTimes = ArrayList<LongArray>()

    /** What the codec reported when it failed. */
    @Volatile var error: String? = null
        private set
    private val callbacks = HandlerThread("decoder", Process.THREAD_PRIORITY_URGENT_DISPLAY).apply {
        start()
        HotThreads.add(threadId)
    }

    init {
        val info = if (level >= 2) pickSoftwareDecoder() else pickDecoder()
        if (onImage != null && level < 2 && info != null && nativeDecoderWanted()) {
            adaptive = try {
                info.getCapabilitiesForType(MediaFormat.MIMETYPE_VIDEO_AVC)
                    .isFeatureSupported(MediaCodecInfo.CodecCapabilities.FEATURE_AdaptivePlayback)
            } catch (_: Exception) { false }
            for (opts in if (level == 0) lowLatencyOptions(info, fps) else listOf(emptyMap())) {
                val all = if (adaptive) opts + mapOf(MediaFormat.KEY_MAX_WIDTH to width, MediaFormat.KEY_MAX_HEIGHT to height) else opts
                native = Native.codecCreate(this, info.name, MediaFormat.MIMETYPE_VIDEO_AVC, width, height,
                    all.keys.toTypedArray(), all.values.toIntArray())
                if (native != 0L) {
                    TLog.i("decoder ${info.name} (setup $level, NDK) started for ${width}x$height with $opts")
                    break
                }
            }
        }
        if (native == 0L) {
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
                    sink(pts, Picture.of(img)) {
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
            // Parts of the screen (a video's window) come at their own size, the whole screen
            // at most: decoders that can, switch between them in place.
            adaptive = try {
                codec.codecInfo.getCapabilitiesForType(MediaFormat.MIMETYPE_VIDEO_AVC)
                    .isFeatureSupported(MediaCodecInfo.CodecCapabilities.FEATURE_AdaptivePlayback)
            } catch (_: Exception) { false }
            if (adaptive) {
                fmt.setInteger(MediaFormat.KEY_MAX_WIDTH, width)
                fmt.setInteger(MediaFormat.KEY_MAX_HEIGHT, height)
            }
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
            HotThreads.remove(callbacks.threadId)
            callbacks.quitSafely()
            throw IllegalStateException("${codec.name} took no configuration")
        }
        }
        name = if (native != 0L) info!!.name else codec.name
    }

    /** From the NDK decoder's thread: a decoded picture (direct buffers over its memory). */
    @Suppress("unused")
    fun onNativeFrame(index: Int, pts: Long, y: java.nio.ByteBuffer, u: java.nio.ByteBuffer, v: java.nio.ByteBuffer,
                      yStride: Int, uvStride: Int, uvStep: Int, cropLeft: Int, cropTop: Int, cropW: Int, cropH: Int) {
        val h = native
        val sink = onImage
        if (!open || sink == null) {
            Native.codecDone(h, index)
            return
        }
        outputs++
        onDecoded(pts)
        synchronized(this) { held++ }
        val pic = Picture(y, u, v, yStride, uvStride, uvStep, android.graphics.Rect(cropLeft, cropTop, cropLeft + cropW, cropTop + cropH))
        sink(pts, pic) {
            Native.codecDone(h, index)
            synchronized(this) {
                held--
                if (nativeClosed && held == 0) Native.codecFree(h)
            }
        }
    }

    /** From the NDK decoder's thread. */
    @Suppress("unused")
    fun onNativeError(message: String) {
        if (message.startsWith("unsupported output layout")) nativeDecoderBroken = true
        error = message
        failed = true
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
        if (native != 0L) {
            val ns = Native.codecFeed(native, au, size, pts, (stuckNanos / 1_000_000).toInt())
            if (ns < 0) {
                if (open && !failed) error = error ?: "the NDK decoder took no input"
                return false
            }
            feedTimes.add(longArrayOf(0, 0, ns))
            if (feedTimes.size >= 300) {
                val q = { f: Double -> feedTimes.map { it[2] }.sorted().let { "%.2f".format(it[((it.size - 1) * f).toInt()] / 1e6) } }
                TLog.i("decoder input (NDK): queueInputBuffer ${q(0.5)}/${q(0.95)} ms (median/p95)")
                feedTimes.clear()
            }
            if (inputs++ == 0) firstInputAt = System.nanoTime()
            return true
        }
        val start = System.nanoTime()
        val deadline = start + stuckNanos
        while (open && !failed && System.nanoTime() < deadline) {
            val i = freeInputs.poll(10, TimeUnit.MILLISECONDS) ?: continue
            val got = System.nanoTime()
            val buf = codec.getInputBuffer(i)!!
            buf.clear()
            buf.put(au, 0, size)
            val filled = System.nanoTime()
            codec.queueInputBuffer(i, 0, size, pts, 0)
            feedTimes.add(longArrayOf(got - start, filled - got, System.nanoTime() - filled))
            if (feedTimes.size >= 300) {
                val q = { k: Int, f: Double -> feedTimes.map { it[k] }.sorted().let { "%.2f".format(it[((it.size - 1) * f).toInt()] / 1e6) } }
                TLog.i("decoder input: waited ${q(0, 0.5)}/${q(0, 0.95)} ms for a buffer, filling ${q(1, 0.5)}/${q(1, 0.95)}, queueInputBuffer ${q(2, 0.5)}/${q(2, 0.95)} ms (median/p95)")
                feedTimes.clear()
            }
            if (inputs++ == 0) firstInputAt = System.nanoTime()
            return true
        }
        if (open && !failed) error = "took no input for ${stuckNanos / 1_000_000} ms"
        return false
    }

    fun close() {
        open = false
        if (native != 0L) {
            Native.codecRelease(native)
            synchronized(this) {
                nativeClosed = true
                if (held == 0) Native.codecFree(native)
            }
            HotThreads.remove(callbacks.threadId)
            callbacks.quitSafely()
            return
        }
        try { codec.stop() } catch (_: Exception) {}
        codec.release()
        HotThreads.remove(callbacks.threadId)
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

    /** What is already here (never asks the driver: Huawei's rejects that ioctl). */
    override fun available(): Int = len - pos

    override fun close() = src.close()
}
