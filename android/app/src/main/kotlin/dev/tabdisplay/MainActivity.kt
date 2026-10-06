package dev.tabdisplay

import android.app.Activity
import android.graphics.Color
import android.os.Bundle
import android.view.Gravity
import android.view.MotionEvent
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.View
import android.view.ViewConfiguration
import android.view.WindowManager
import android.widget.FrameLayout
import android.widget.LinearLayout
import android.widget.Switch
import android.widget.TextView
import kotlin.math.roundToInt

class MainActivity : Activity() {
    companion object {
        /** The CPU display paths failed on this device: plain video until the app restarts. */
        @Volatile var forcePlainVideo = false
    }

    private lateinit var surfaceView: SurfaceView
    private lateinit var status: TextView
    private lateinit var panel: LinearLayout
    private lateinit var audioSwitch: Switch
    private lateinit var touchSwitch: Switch
    private lateinit var hostCursor: HostCursor
    private lateinit var latencySwitch: Switch
    /** Draws video and cursor into the scanned-out buffer; null when off or unsupported. */
    private var front: Renderer? = null
    private var tapDownAt = 0L
    private var session: Session? = null
    private val hidePanel = Runnable { panel.visibility = View.GONE }
    @Volatile private var refreshHz = 60

    /** Experiment (variant F): redraw the (transparent) window every frame while streaming. */
    private var keepAwake = false
    @Volatile private var streaming = false
    private val keepAlive by lazy {
        object : View(this) {
            private val paint = android.graphics.Paint()
            private var tick = false
            override fun onDraw(c: android.graphics.Canvas) {
                if (!keepAwake || !streaming) return
                // One pixel at alpha 0 or 1/255: invisible, but a new frame with full damage.
                tick = !tick
                paint.color = if (tick) 0x01000000 else 0
                c.drawRect(0f, 0f, 1f, 1f, paint)
                postInvalidateOnAnimation()
            }
        }
    }

    /**
     * The panel's modes change at runtime (battery saver, motion smoothness: the host lifts
     * those caps after the app may already be open), so pick again whenever the display changes.
     */
    private val displayListener = object : android.hardware.display.DisplayManager.DisplayListener {
        override fun onDisplayAdded(id: Int) {}
        override fun onDisplayRemoved(id: Int) {}
        override fun onDisplayChanged(id: Int) {
            if (id != display?.displayId) return
            refreshHz = pickFastestDisplayMode()
            session?.panelChanged()
        }
    }

    /**
     * Asks for the highest refresh rate the panel offers at its current resolution.
     * At 120 Hz a decoded frame waits at most ~8 ms for the next vsync instead of ~17 ms,
     * and the Mac can stream at that rate too.
     */
    private fun pickFastestDisplayMode(): Int {
        val d = display ?: return 60
        val cur = d.mode
        val best = d.supportedModes
            .filter { it.physicalWidth == cur.physicalWidth && it.physicalHeight == cur.physicalHeight }
            .maxByOrNull { it.refreshRate } ?: cur
        if (window.attributes.preferredDisplayModeId != best.modeId) {
            window.attributes = window.attributes.also {
                it.preferredDisplayModeId = best.modeId
                it.preferredRefreshRate = best.refreshRate
            }
        }
        return best.refreshRate.roundToInt()
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        refreshHz = pickFastestDisplayMode()
        if (intent.getBooleanExtra("yuvprobe", false)) { // PROBE
            val HB = android.hardware.HardwareBuffer::class.java
            for ((name, usage) in listOf(
                "overlay+cpu" to (android.hardware.HardwareBuffer.USAGE_COMPOSER_OVERLAY or android.hardware.HardwareBuffer.USAGE_CPU_WRITE_RARELY),
                "overlay+cpu+gpu" to (android.hardware.HardwareBuffer.USAGE_COMPOSER_OVERLAY or android.hardware.HardwareBuffer.USAGE_CPU_WRITE_RARELY or android.hardware.HardwareBuffer.USAGE_GPU_SAMPLED_IMAGE),
            )) {
                val ok = android.hardware.HardwareBuffer.isSupported(2304, 1440, android.hardware.HardwareBuffer.YCBCR_420_888, 1, usage)
                android.util.Log.i("tabdisplay", "yuv probe $name supported=$ok ${HB.simpleName}")
                if (ok) android.hardware.HardwareBuffer.create(2304, 1440, android.hardware.HardwareBuffer.YCBCR_420_888, 1, usage).use { Native.probeYuv(it) }
            }
        }
        getSystemService(android.hardware.display.DisplayManager::class.java)
            .registerDisplayListener(displayListener, android.os.Handler(mainLooper))

        surfaceView = SurfaceView(this)

        status = TextView(this).apply {
            text = "Waiting for the computer…\nRun tabdisplay-host and keep the USB cable connected."
            setTextColor(Color.LTGRAY)
            textSize = 18f
            gravity = Gravity.CENTER
            setBackgroundColor(Color.BLACK)
        }

        audioSwitch = Switch(this).apply {
            text = "Audio  "
            setTextColor(Color.WHITE)
            textSize = 18f
            setOnCheckedChangeListener { _, on ->
                session?.setAudio(on)
                scheduleHide()
            }
        }
        // Off by default: the tablet is a display for the computer's own mouse. When on, touches
        // and the pen drive the mouse and a three-finger tap brings this panel back.
        val prefs = getPreferences(MODE_PRIVATE)
        touchSwitch = Switch(this).apply {
            text = "Touch controls mouse  "
            setTextColor(Color.WHITE)
            textSize = 18f
            isChecked = prefs.getBoolean("touch", false)
            setOnCheckedChangeListener { _, on ->
                prefs.edit().putBoolean("touch", on).apply()
                scheduleHide()
            }
        }
        // On by default where the hardware allows it: skips the compositor's queue (~17 ms at
        // 90 Hz) at the cost of possible tearing. Switching rebuilds the activity.
        val frontSupported = YuvFront.supported()
        // For measuring: `am start ... --ez front false` sets the switch.
        if (intent.hasExtra("front")) prefs.edit().putBoolean("front", intent.getBooleanExtra("front", true)).commit()
        latencySwitch = Switch(this).apply {
            text = if (frontSupported) "Lowest latency  " else "Lowest latency (needs Android 13)  "
            setTextColor(Color.WHITE)
            textSize = 18f
            isEnabled = frontSupported
            isChecked = frontSupported && prefs.getBoolean("front", true)
            setOnCheckedChangeListener { _, on ->
                prefs.edit().putBoolean("front", on).apply()
                recreate()
            }
        }
        panel = LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            setPadding(40, 24, 40, 24)
            setBackgroundColor(0xCC000000.toInt())
            addView(latencySwitch)
            addView(touchSwitch)
            addView(audioSwitch)
            visibility = View.GONE
        }

        val root = FrameLayout(this)
        root.addView(surfaceView)
        root.addView(keepAlive)
        val cursor = CursorOverlay(this)
        root.addView(cursor)
        root.addView(status)
        root.addView(
            panel,
            FrameLayout.LayoutParams(
                FrameLayout.LayoutParams.WRAP_CONTENT,
                FrameLayout.LayoutParams.WRAP_CONTENT,
                Gravity.TOP or Gravity.END,
            ).apply { setMargins(24, 24, 24, 24) },
        )
        setContentView(root)
        hideSystemBars()

        hostCursor = HostCursor(surfaceView)
        run {
            val changed = { pts: Long -> session?.takeChangedArea(pts) }
            val shown = { pts: Long, nanos: Long -> session?.frameShown(pts, nanos); Unit }
            // The CPU writes into the scanned-out buffer where the hardware allows it; else the GPU does.
            // Experiment: `--ei variant` 1 = CPU, 2 = CPU with a GPU-allocated buffer, 3 = GL.
            val variant = if (latencySwitch.isChecked) intent.getIntExtra("variant", 0) else 0
            CpuFront.gpuUsage = variant == 2
            CpuFront.singleBuffer = variant in 1..6
            CpuFront.noFrontFlag = variant == 4 || variant == 8
            CpuFront.noDamage = variant == 5 || variant == 6
            keepAwake = variant == 6
            front = when {
                variant == 3 -> FrontRenderer(surfaceView, changed, shown)
                variant in 1..8 && CpuFront.supported() -> CpuRenderer(surfaceView, changed, shown).also { it.useFrontBuffer = true }
                // 9: the compositor-paced NV12 chain instead of the front buffer.
                forcePlainVideo -> null
                YuvFront.supported() || YuvChain.supported() || NdkChain.supported() || SwapChain.supported() ->
                    CpuRenderer(surfaceView, changed, shown).also {
                        // The switch picks the front buffer; without it (or before Android 13)
                        // the compositor-paced NV12 swap chain.
                        it.lowestLatency = latencySwitch.isChecked && variant != 9
                        it.onUnusable = { runOnUiThread { forcePlainVideo = true; recreate() } }
                    }
                else -> null // the plain video path
            }
            TLog.i(
                "Android ${android.os.Build.VERSION.RELEASE} (API ${android.os.Build.VERSION.SDK_INT}), " +
                    "${android.os.Build.MANUFACTURER} ${android.os.Build.MODEL}: renderer " +
                    when {
                        front is CpuRenderer && latencySwitch.isChecked && YuvFront.supported() -> "front buffer (lowest latency)"
                        front is CpuRenderer -> "NV12 swap chain"
                        front != null -> front!!::class.simpleName
                        forcePlainVideo -> "plain video (the fast paths failed on this device)"
                        else -> "plain video (this device cannot hand CPU-written buffers to the compositor)"
                    }
            )
            // Debugging: `adb shell am broadcast -a dev.tabdisplay.DUMP` saves what the panel shows.
            registerReceiver(object : android.content.BroadcastReceiver() {
                override fun onReceive(c: android.content.Context, i: android.content.Intent) {
                    (front as? CpuRenderer)?.dump(java.io.File(getExternalFilesDir(null), "front.png"))
                }
            }, android.content.IntentFilter("dev.tabdisplay.DUMP"), RECEIVER_EXPORTED)
        }

        // With touch input off, a tap shows the panel for a few seconds. With it on, touches and
        // the pen control the computer's mouse and a three-finger tap shows the panel.
        val touch = TouchInput(
            send = { a, x, y -> session?.sendPointer(a, x, y) },
            scroll = { dx, dy -> session?.sendScroll(dx, dy) },
            onThreeFingerTap = {
                panel.visibility = View.VISIBLE
                scheduleHide()
            },
            touchSlop = ViewConfiguration.get(this).scaledTouchSlop.toFloat(),
        )
        surfaceView.addOnLayoutChangeListener { v, _, _, _, _, _, _, _, _ -> touch.setViewSize(v.width, v.height) }
        surfaceView.setOnTouchListener { _, e ->
            if (touchSwitch.isChecked) {
                cursor.onPen(e)
                touch.onTouch(e)
            } else {
                showPanelOnTap(e)
            }
        }
        surfaceView.setOnGenericMotionListener { _, e ->
            touchSwitch.isChecked && run { cursor.onPen(e); touch.onHover(e) }
        }
        surfaceView.holder.addCallback(object : SurfaceHolder.Callback {
            override fun surfaceCreated(h: SurfaceHolder) {
                front?.start()
            }
            override fun surfaceChanged(h: SurfaceHolder, f: Int, w: Int, hh: Int) {}
            override fun surfaceDestroyed(h: SurfaceHolder) = hostCursor.release()
        })
    }

    private fun showPanelOnTap(e: MotionEvent): Boolean {
        when (e.actionMasked) {
            MotionEvent.ACTION_DOWN -> tapDownAt = e.eventTime
            MotionEvent.ACTION_UP -> if (e.eventTime - tapDownAt < 300) {
                panel.visibility = View.VISIBLE
                scheduleHide()
            }
        }
        return true
    }

    private fun scheduleHide() {
        panel.removeCallbacks(hidePanel)
        panel.postDelayed(hidePanel, 4000)
    }

    override fun onStart() {
        super.onStart()
        val bounds = windowManager.maximumWindowMetrics.bounds
        val w = maxOf(bounds.width(), bounds.height())
        val h = minOf(bounds.width(), bounds.height())
        session = Session(
            port = 27183,
            screenW = w,
            screenH = h,
            maxFps = {
                maxDecodableFps(w, h, refreshHz).also {
                    android.util.Log.i("tabdisplay", "panel ${w}x$h @ $refreshHz Hz, stream up to $it fps")
                }
            },
            panelHz = { refreshHz },
            surfaceProvider = { front?.surface ?: surfaceView.holder.surface.takeIf { front == null } },
            toTexture = front != null,
            front = front,
            usb = getSystemService(android.hardware.usb.UsbManager::class.java),
            askUsbPermission = { acc ->
                val pi = android.app.PendingIntent.getBroadcast(this, 0, android.content.Intent("dev.tabdisplay.USB")
                    .setPackage(packageName), android.app.PendingIntent.FLAG_IMMUTABLE)
                getSystemService(android.hardware.usb.UsbManager::class.java).requestPermission(acc, pi)
            },
            onState = { connected ->
                runOnUiThread {
                    status.visibility = if (connected) View.GONE else View.VISIBLE
                    streaming = connected
                    keepAlive.invalidate()
                }
            },
            onCursor = front?.let { it::moveCursor } ?: hostCursor::move,
            onCursorImage = front?.let { f -> { msg: ByteArray -> HostCursor.parseImage(msg)?.let { f.setCursorImage(it.bitmap, it.displayWidthPt, it.sizePt, it.hotPt) } } }
                ?: hostCursor::setImage,
        ).also { it.setAudio(audioSwitch.isChecked) }
        // Debugging: `--ei decoder N` starts at decoder setup N (1 plain, 2 software).
        session?.decoderLevel = intent.getIntExtra("decoder", 0)
    }

    override fun onDestroy() {
        getSystemService(android.hardware.display.DisplayManager::class.java).unregisterDisplayListener(displayListener)
        front?.release()
        super.onDestroy()
    }

    override fun onStop() {
        session?.stop()
        session = null
        super.onStop()
    }

    /** The video gets the whole panel, pixel for pixel: no status bar, navigation bar or taskbar. */
    private fun hideSystemBars() {
        window.setDecorFitsSystemWindows(false)
        window.attributes = window.attributes.also {
            it.layoutInDisplayCutoutMode = WindowManager.LayoutParams.LAYOUT_IN_DISPLAY_CUTOUT_MODE_ALWAYS
        }
        window.insetsController?.apply {
            hide(android.view.WindowInsets.Type.systemBars())
            systemBarsBehavior = android.view.WindowInsetsController.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE
        }
    }

    override fun onWindowFocusChanged(hasFocus: Boolean) {
        super.onWindowFocusChanged(hasFocus)
        if (hasFocus) hideSystemBars()
    }
}
