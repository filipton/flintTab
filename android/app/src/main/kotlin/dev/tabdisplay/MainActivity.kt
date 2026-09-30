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
    private lateinit var surfaceView: SurfaceView
    private lateinit var status: TextView
    private lateinit var panel: LinearLayout
    private lateinit var audioSwitch: Switch
    private lateinit var touchSwitch: Switch
    private lateinit var hostCursor: HostCursor
    private var tapDownAt = 0L
    private var session: Session? = null
    private val hidePanel = Runnable { panel.visibility = View.GONE }
    private var refreshHz = 60

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
        window.attributes = window.attributes.also { it.preferredDisplayModeId = best.modeId }
        return best.refreshRate.roundToInt()
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        refreshHz = pickFastestDisplayMode()

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
        panel = LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            setPadding(40, 24, 40, 24)
            setBackgroundColor(0xCC000000.toInt())
            addView(touchSwitch)
            addView(audioSwitch)
            visibility = View.GONE
        }

        val root = FrameLayout(this)
        root.addView(surfaceView)
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

        hostCursor = HostCursor(surfaceView)

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
            override fun surfaceCreated(h: SurfaceHolder) {}
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
            maxFps = maxDecodableFps(w, h, refreshHz),
            surfaceProvider = { surfaceView.holder.surface },
            onState = { connected -> runOnUiThread { status.visibility = if (connected) View.GONE else View.VISIBLE } },
            onCursor = hostCursor::move,
            onCursorImage = hostCursor::setImage,
        ).also { it.setAudio(audioSwitch.isChecked) }
    }

    override fun onStop() {
        session?.stop()
        session = null
        super.onStop()
    }

    override fun onWindowFocusChanged(hasFocus: Boolean) {
        super.onWindowFocusChanged(hasFocus)
        if (hasFocus) {
            window.decorView.systemUiVisibility = (View.SYSTEM_UI_FLAG_FULLSCREEN
                or View.SYSTEM_UI_FLAG_HIDE_NAVIGATION
                or View.SYSTEM_UI_FLAG_IMMERSIVE_STICKY
                or View.SYSTEM_UI_FLAG_LAYOUT_STABLE)
        }
    }
}
