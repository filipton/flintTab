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
    private lateinit var hostCursor: HostCursor
    /** The host's --touch: touches and the pen drive the computer's mouse. */
    @Volatile private var touchEnabled = false
    /** Draws video and cursor into the scanned-out buffer; null when off or unsupported. */
    private var front: Renderer? = null
    private var session: Session? = null
    @Volatile private var refreshHz = 60

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
        CrashLog.install(this)
        // Testing the crash reports: `--ez crash true` (Kotlin) or `--ez panic true` (native).
        if (intent.getBooleanExtra("crash", false)) Thread { Thread.sleep(3000); error("crash test") }.start()
        if (intent.getBooleanExtra("panic", false)) Thread { Thread.sleep(3000); Native.panicTest() }.start()
        window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        refreshHz = pickFastestDisplayMode()
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

        val root = FrameLayout(this)
        root.addView(surfaceView)
        val cursor = CursorOverlay(this)
        root.addView(cursor)
        root.addView(status)
        setContentView(root)
        hideSystemBars()

        hostCursor = HostCursor(surfaceView)
        run {
            val changed = { pts: Long -> session?.takeChangedArea(pts) }
            val shown = { pts: Long, nanos: Long -> session?.frameShown(pts, nanos); Unit }
            // NV12 into a buffer the display scans out (front buffer, Android 13+, when the host
            // asks for lowest latency) or into a compositor-paced swap chain; plain video if this
            // device can do neither.
            front = when {
                forcePlainVideo -> null
                YuvFront.supported() || YuvChain.supported() || NdkChain.supported() || SwapChain.supported() ->
                    CpuRenderer(surfaceView, changed, shown).also {
                        it.regionOf = { pts -> session?.takeRegion(pts) }
                        it.onNeedKeyframe = { session?.requestKeyframe() }
                        it.onUnusable = { runOnUiThread { forcePlainVideo = true; recreate() } }
                    }
                else -> null
            }
            TLog.i(
                "Android ${android.os.Build.VERSION.RELEASE} (API ${android.os.Build.VERSION.SDK_INT}), " +
                    "${android.os.Build.MANUFACTURER} ${android.os.Build.MODEL}: renderer " +
                    when {
                        front != null -> "NV12 (front buffer: ${if (YuvFront.supported() || NdkFront.supported()) "available" else "needs Android 10"})"
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

        // With the host's --touch, touches and the pen control the computer's mouse.
        val touch = TouchInput(
            send = { a, x, y -> session?.sendPointer(a, x, y) },
            scroll = { dx, dy -> session?.sendScroll(dx, dy) },
            onThreeFingerTap = {},
            touchSlop = ViewConfiguration.get(this).scaledTouchSlop.toFloat(),
        )
        surfaceView.addOnLayoutChangeListener { v, _, _, _, _, _, _, _, _ -> touch.setViewSize(v.width, v.height) }
        surfaceView.setOnTouchListener { _, e ->
            touchEnabled && run {
                cursor.onPen(e)
                touch.onTouch(e)
            }
        }
        surfaceView.setOnGenericMotionListener { _, e ->
            touchEnabled && run { cursor.onPen(e); touch.onHover(e) }
        }
        surfaceView.holder.addCallback(object : SurfaceHolder.Callback {
            override fun surfaceCreated(h: SurfaceHolder) {
                front?.start()
            }
            override fun surfaceChanged(h: SurfaceHolder, f: Int, w: Int, hh: Int) {}
            override fun surfaceDestroyed(h: SurfaceHolder) = hostCursor.release()
        })
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
                }
            },
            onCursor = front?.let { it::moveCursor } ?: hostCursor::move,
            onCursorImage = front?.let { f -> { msg: ByteArray -> HostCursor.parseImage(msg)?.let { f.setCursorImage(it.bitmap, it.displayWidthPt, it.sizePt, it.hotPt) } } }
                ?: hostCursor::setImage,
            onSettings = { flags ->
                touchEnabled = flags and Session.SETTING_TOUCH != 0
                (front as? CpuRenderer)?.chooseLowestLatency(flags and Session.SETTING_LOWEST_LATENCY != 0)
            },
        )
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
