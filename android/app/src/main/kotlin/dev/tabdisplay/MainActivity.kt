package dev.tabdisplay

import android.app.Activity
import android.graphics.Color
import android.os.Bundle
import android.view.Gravity
import android.view.MotionEvent
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.View
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
            text = "Waiting for the Mac…\nRun tabdisplay-host and keep the USB cable connected."
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
        panel = LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            setPadding(40, 24, 40, 24)
            setBackgroundColor(0xCC000000.toInt())
            addView(audioSwitch)
            visibility = View.GONE
        }

        val root = FrameLayout(this)
        root.addView(surfaceView)
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

        // A tap shows the audio switch for a few seconds.
        surfaceView.setOnTouchListener { _, e ->
            if (e.action == MotionEvent.ACTION_DOWN) {
                panel.visibility = View.VISIBLE
                scheduleHide()
            }
            true
        }
        surfaceView.holder.addCallback(object : SurfaceHolder.Callback {
            override fun surfaceCreated(h: SurfaceHolder) {}
            override fun surfaceChanged(h: SurfaceHolder, f: Int, w: Int, hh: Int) {}
            override fun surfaceDestroyed(h: SurfaceHolder) {}
        })
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
