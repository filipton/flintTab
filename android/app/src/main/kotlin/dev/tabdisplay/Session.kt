package dev.tabdisplay

import android.media.AudioAttributes
import android.media.AudioFormat
import android.media.AudioTrack
import android.media.MediaCodec
import android.media.MediaCodecInfo
import android.media.MediaCodecList
import android.media.MediaFormat
import android.os.Process
import android.view.Surface
import java.io.DataInputStream
import java.io.DataOutputStream
import java.net.InetSocketAddress
import java.net.Socket
import java.util.concurrent.LinkedBlockingDeque
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
    private val maxFps: Int,
    private val surfaceProvider: () -> Surface?,
    private val onState: (connected: Boolean) -> Unit,
) {
    @Volatile private var running = true
    @Volatile private var socket: Socket? = null
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

    fun stop() {
        running = false
        try { socket?.close() } catch (_: Exception) {}
        worker.join(1500)
    }

    private fun sendControl(kind: Int, value: Int) {
        val o = out ?: return
        try {
            synchronized(o) {
                o.writeByte(kind); o.writeByte(value); o.flush()
            }
        } catch (_: Exception) {}
    }

    private fun loop() {
        while (running) {
            val surface = surfaceProvider()
            if (surface == null || !surface.isValid) { Thread.sleep(100); continue }
            try {
                runOnce(surface)
            } catch (_: Exception) {
                // connection refused / dropped: retry below
            }
            onState(false)
            if (running) Thread.sleep(500)
        }
    }

    private fun runOnce(surface: Surface) {
        val s = Socket()
        s.tcpNoDelay = true
        s.connect(InetSocketAddress("127.0.0.1", port), 1000)
        socket = s
        val input = DataInputStream(s.getInputStream().buffered(1 shl 16))
        val output = DataOutputStream(s.getOutputStream())
        out = output

        // handshake: magic, version, screen size
        synchronized(output) {
            output.write("TDSP".toByteArray())
            output.writeByte(VERSION)
            output.writeInt(screenW)
            output.writeInt(screenH)
            output.writeInt(maxFps)
            output.flush()
        }

        var decoder: Decoder? = null
        var config: IntArray? = null // w, h, fps
        var needKeyframe = false
        var frameBuf = ByteArray(1 shl 20) // reused: no per-frame allocation/GC
        try {
            while (running) {
                val kind = input.readUnsignedByte()
                val len = input.readInt()
                when (kind) {
                    MSG_CONFIG -> {
                        val w = input.readInt(); val h = input.readInt(); val fps = input.readInt()
                        val rate = input.readInt(); val ch = input.readUnsignedByte()
                        decoder?.close()
                        audio?.close()
                        config = intArrayOf(w, h, fps)
                        // Let the display switch to a refresh rate that fits the stream.
                        try { surface.setFrameRate(fps.toFloat(), Surface.FRAME_RATE_COMPATIBILITY_FIXED_SOURCE) } catch (_: Exception) {}
                        decoder = Decoder(surface, w, h, fps)
                        needKeyframe = false
                        audio = AudioPlayer(rate, ch).also { it.setEnabled(audioWanted) }
                        // host starts with audio off; tell it what the switch currently says
                        sendControl(KIND_AUDIO, if (audioWanted) 1 else 0)
                        onState(true)
                    }
                    MSG_VIDEO -> {
                        input.readLong() // pts, unused: frames are shown as soon as decoded
                        val n = len - 8
                        if (n > frameBuf.size) frameBuf = ByteArray(n * 2)
                        input.readFully(frameBuf, 0, n)
                        if (needKeyframe && isKeyframe(frameBuf, n)) needKeyframe = false
                        val d = decoder
                        if (d != null && !needKeyframe) {
                            val ok = !d.failed && try { d.feed(frameBuf, n); true } catch (_: Exception) { false }
                            if (!ok) {
                                // Decoder died (e.g. a codec error): rebuild it and ask the host for
                                // a keyframe instead of tearing the whole connection down.
                                d.close()
                                val c = config!!
                                decoder = Decoder(surface, c[0], c[1], c[2])
                                needKeyframe = true
                                sendControl(KIND_IDR, 0)
                            }
                        }
                        // Flow control: the host keeps at most a couple of frames unacknowledged.
                        sendControl(KIND_ACK, 0)
                    }
                    MSG_AUDIO -> {
                        val data = ByteArray(len)
                        input.readFully(data)
                        audio?.enqueue(data)
                    }
                    else -> input.skipBytes(len)
                }
            }
        } finally {
            decoder?.close()
            audio?.close()
            audio = null
            out = null
            try { s.close() } catch (_: Exception) {}
        }
    }

    companion object {
        const val VERSION = 2
        const val MSG_CONFIG = 1
        const val MSG_VIDEO = 2
        const val MSG_AUDIO = 3
        const val KIND_AUDIO = 1
        const val KIND_ACK = 2
        const val KIND_IDR = 3
    }
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
    for (fps in listOf(refreshHz, 120, 90, 60).filter { it <= refreshHz }.distinct()) {
        if (caps.any { it.areSizeAndRateSupported(width, height, fps.toDouble()) }) return fps
    }
    return 60
}

private fun createLowLatencyDecoder(): MediaCodec {
    // Prefer a hardware decoder that advertises low-latency support.
    val info = MediaCodecList(MediaCodecList.REGULAR_CODECS).codecInfos.firstOrNull { c ->
        !c.isEncoder && !c.isSoftwareOnly &&
            c.supportedTypes.contains(MediaFormat.MIMETYPE_VIDEO_AVC) &&
            c.getCapabilitiesForType(MediaFormat.MIMETYPE_VIDEO_AVC)
                .isFeatureSupported(MediaCodecInfo.CodecCapabilities.FEATURE_LowLatency)
    }
    return if (info != null) MediaCodec.createByCodecName(info.name)
    else MediaCodec.createDecoderByType(MediaFormat.MIMETYPE_VIDEO_AVC)
}

/** H.264 decoder configured for minimum latency, rendering straight to the surface. */
private class Decoder(surface: Surface, width: Int, height: Int, fps: Int) {
    private val codec = createLowLatencyDecoder()
    @Volatile private var open = true
    /** Set when the codec threw on the output side; the owner then rebuilds it. */
    @Volatile var failed = false
        private set
    private val drain: Thread

    init {
        val fmt = MediaFormat.createVideoFormat(MediaFormat.MIMETYPE_VIDEO_AVC, width, height)
        fmt.setInteger(MediaFormat.KEY_LOW_LATENCY, 1)
        fmt.setInteger(MediaFormat.KEY_PRIORITY, 0) // real-time
        fmt.setInteger(MediaFormat.KEY_OPERATING_RATE, fps)
        // Vendor hints (ignored where unsupported)
        fmt.setInteger("vendor.qti-ext-dec-low-latency.enable", 1)
        fmt.setInteger("vendor.rtc-ext-dec-low-latency.enable", 1)
        codec.configure(fmt, surface, null, 0)
        codec.start()

        drain = thread(name = "decoder-out", isDaemon = true) {
            Process.setThreadPriority(Process.THREAD_PRIORITY_URGENT_DISPLAY)
            val info = MediaCodec.BufferInfo()
            while (open) {
                try {
                    var idx = codec.dequeueOutputBuffer(info, 10_000)
                    if (idx < 0) continue
                    // If newer frames are already waiting, skip the older ones.
                    while (true) {
                        val next = codec.dequeueOutputBuffer(info, 0)
                        if (next < 0) break
                        codec.releaseOutputBuffer(idx, false)
                        idx = next
                    }
                    codec.releaseOutputBuffer(idx, true)
                } catch (_: Exception) {
                    if (open) failed = true
                    return@thread
                }
            }
        }
    }

    fun feed(au: ByteArray, size: Int) {
        val pts = System.nanoTime() / 1000
        while (open) {
            val i = codec.dequeueInputBuffer(10_000)
            if (i < 0) continue
            val buf = codec.getInputBuffer(i)!!
            buf.clear()
            buf.put(au, 0, size)
            codec.queueInputBuffer(i, 0, size, pts, 0)
            return
        }
    }

    fun close() {
        open = false
        drain.join(500)
        try { codec.stop() } catch (_: Exception) {}
        codec.release()
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
