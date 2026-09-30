package dev.tabdisplay

import android.content.Context
import android.graphics.Canvas
import android.graphics.Color
import android.graphics.Paint
import android.view.MotionEvent
import android.view.View

/**
 * A pen cursor drawn by the tablet itself, on top of the video (Duet does the same).
 * It follows the pen at the display's own refresh rate, so aiming feels immediate while the
 * computer's real cursor catches up in the video a few tens of milliseconds later.
 */
class CursorOverlay(context: Context) : View(context) {
    private var x = 0f
    private var y = 0f
    private var shown = false
    private val density = resources.displayMetrics.density
    private val ring = Paint(Paint.ANTI_ALIAS_FLAG).apply {
        style = Paint.Style.STROKE
        strokeWidth = 1.5f * density
        color = Color.WHITE
    }
    private val halo = Paint(Paint.ANTI_ALIAS_FLAG).apply {
        style = Paint.Style.STROKE
        strokeWidth = 3.5f * density
        color = 0x99000000.toInt()
    }
    private val hide = Runnable { shown = false; invalidate() }

    init {
        isClickable = false
        isFocusable = false
    }

    /** Feed every pen event (hover and contact); fingers are ignored. */
    fun onPen(e: MotionEvent) {
        val t = e.getToolType(0)
        if (t != MotionEvent.TOOL_TYPE_STYLUS && t != MotionEvent.TOOL_TYPE_ERASER) return
        x = e.x; y = e.y
        removeCallbacks(hide)
        if (e.actionMasked == MotionEvent.ACTION_HOVER_EXIT) postDelayed(hide, 150) else shown = true
        invalidate()
    }

    override fun onDraw(canvas: Canvas) {
        if (!shown) return
        val r = 5f * density
        canvas.drawCircle(x, y, r, halo)
        canvas.drawCircle(x, y, r, ring)
    }
}
