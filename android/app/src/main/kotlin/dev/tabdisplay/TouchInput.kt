package dev.tabdisplay

import android.os.SystemClock
import android.view.MotionEvent
import kotlin.math.hypot

/**
 * Turns touches and pen strokes on the stream into mouse input for the computer:
 *
 * - one finger: tap = click, drag = click-and-drag (the press is held back until the finger
 *   moves or ~100 ms pass, so a second finger can still turn it into a gesture)
 * - two fingers: drag = scroll, tap = right click
 * - three fingers: tap = show the settings panel
 * - pen: goes straight through, hover moves the cursor, the barrel button right-clicks
 */
class TouchInput(
    private val send: (action: Int, x: Int, y: Int) -> Unit,
    private val scroll: (dx: Int, dy: Int) -> Unit,
    private val onThreeFingerTap: () -> Unit,
    private val touchSlop: Float,
) {
    private var viewW = 1f
    private var viewH = 1f

    private enum class Mode { IDLE, PENDING, DRAG, MULTI, PEN }
    private var mode = Mode.IDLE
    private var penButton = LEFT
    private var downX = 0f
    private var downY = 0f
    private var downTime = 0L
    private var maxPointers = 0
    private var multiMoved = false
    private var lastCx = 0f
    private var lastCy = 0f
    private var startCx = 0f
    private var startCy = 0f
    private var accX = 0f
    private var accY = 0f

    fun setViewSize(w: Int, h: Int) {
        viewW = maxOf(1, w).toFloat()
        viewH = maxOf(1, h).toFloat()
    }

    private fun nx(x: Float) = (x / viewW * 65535f).toInt().coerceIn(0, 65535)
    private fun ny(y: Float) = (y / viewH * 65535f).toInt().coerceIn(0, 65535)
    private fun emit(action: Int, x: Float, y: Float) = send(action, nx(x), ny(y))

    private fun isPen(e: MotionEvent, i: Int = 0): Boolean {
        val t = e.getToolType(i)
        return t == MotionEvent.TOOL_TYPE_STYLUS || t == MotionEvent.TOOL_TYPE_ERASER
    }

    /** Pen hover (from View.setOnGenericMotionListener). */
    fun onHover(e: MotionEvent): Boolean {
        if (e.actionMasked == MotionEvent.ACTION_HOVER_MOVE || e.actionMasked == MotionEvent.ACTION_HOVER_ENTER) {
            emit(Proto.POINTER_MOVE, e.x, e.y)
            return true
        }
        return false
    }

    fun onTouch(e: MotionEvent): Boolean {
        when (e.actionMasked) {
            MotionEvent.ACTION_DOWN -> {
                downX = e.x; downY = e.y; downTime = SystemClock.uptimeMillis()
                maxPointers = 1
                if (isPen(e)) {
                    mode = Mode.PEN
                    val barrel = e.buttonState and MotionEvent.BUTTON_STYLUS_PRIMARY != 0
                    penButton = if (barrel) RIGHT else LEFT
                    emit(Proto.POINTER_MOVE, e.x, e.y)
                    emit(if (penButton == RIGHT) Proto.POINTER_RIGHT_DOWN else Proto.POINTER_LEFT_DOWN, e.x, e.y)
                } else {
                    mode = Mode.PENDING
                    emit(Proto.POINTER_MOVE, e.x, e.y) // cursor jumps under the finger at once
                }
            }
            MotionEvent.ACTION_POINTER_DOWN -> {
                maxPointers = maxOf(maxPointers, e.pointerCount)
                if (mode == Mode.PENDING || mode == Mode.MULTI) {
                    if (mode == Mode.PENDING) {
                        multiMoved = false
                        accX = 0f; accY = 0f
                        startCx = centroidX(e); startCy = centroidY(e)
                    }
                    mode = Mode.MULTI
                    lastCx = centroidX(e); lastCy = centroidY(e)
                }
            }
            MotionEvent.ACTION_POINTER_UP -> if (mode == Mode.MULTI) {
                // the centroid jumps when a finger lifts: measure from the remaining ones
                val up = e.actionIndex
                var sx = 0f; var sy = 0f; var n = 0
                for (i in 0 until e.pointerCount) if (i != up) { sx += e.getX(i); sy += e.getY(i); n++ }
                if (n > 0) { lastCx = sx / n; lastCy = sy / n }
            }
            MotionEvent.ACTION_MOVE -> when (mode) {
                Mode.PEN -> emit(Proto.POINTER_DRAG, e.x, e.y)
                Mode.PENDING -> {
                    val moved = hypot(e.x - downX, e.y - downY) > touchSlop
                    val held = SystemClock.uptimeMillis() - downTime > HOLD_MS
                    if (moved || held) {
                        mode = Mode.DRAG
                        emit(Proto.POINTER_LEFT_DOWN, downX, downY)
                        emit(Proto.POINTER_DRAG, e.x, e.y)
                    }
                }
                Mode.DRAG -> emit(Proto.POINTER_DRAG, e.x, e.y)
                Mode.MULTI -> if (e.pointerCount >= 2) {
                    val cx = centroidX(e); val cy = centroidY(e)
                    if (!multiMoved && hypot(cx - startCx, cy - startCy) > touchSlop) multiMoved = true
                    if (multiMoved) {
                        accX += cx - lastCx; accY += cy - lastCy
                        val ix = accX.toInt(); val iy = accY.toInt()
                        if (ix != 0 || iy != 0) {
                            scroll(ix, iy)
                            accX -= ix; accY -= iy
                        }
                    }
                    lastCx = cx; lastCy = cy
                }
                Mode.IDLE -> {}
            }
            MotionEvent.ACTION_UP -> {
                when (mode) {
                    Mode.PEN -> emit(if (penButton == RIGHT) Proto.POINTER_RIGHT_UP else Proto.POINTER_LEFT_UP, e.x, e.y)
                    Mode.PENDING -> { // a quick tap: click
                        emit(Proto.POINTER_LEFT_DOWN, downX, downY)
                        emit(Proto.POINTER_LEFT_UP, downX, downY)
                    }
                    Mode.DRAG -> emit(Proto.POINTER_LEFT_UP, e.x, e.y)
                    Mode.MULTI -> if (!multiMoved && SystemClock.uptimeMillis() - downTime < TAP_MS) {
                        if (maxPointers >= 3) {
                            onThreeFingerTap()
                        } else {
                            emit(Proto.POINTER_RIGHT_DOWN, downX, downY)
                            emit(Proto.POINTER_RIGHT_UP, downX, downY)
                        }
                    }
                    Mode.IDLE -> {}
                }
                mode = Mode.IDLE
            }
            MotionEvent.ACTION_CANCEL -> {
                if (mode == Mode.DRAG) emit(Proto.POINTER_LEFT_UP, e.x, e.y)
                if (mode == Mode.PEN) emit(if (penButton == RIGHT) Proto.POINTER_RIGHT_UP else Proto.POINTER_LEFT_UP, e.x, e.y)
                mode = Mode.IDLE
            }
        }
        return true
    }

    private fun centroidX(e: MotionEvent): Float {
        var s = 0f
        for (i in 0 until e.pointerCount) s += e.getX(i)
        return s / e.pointerCount
    }

    private fun centroidY(e: MotionEvent): Float {
        var s = 0f
        for (i in 0 until e.pointerCount) s += e.getY(i)
        return s / e.pointerCount
    }

    companion object {
        private const val LEFT = 0
        private const val RIGHT = 1
        private const val HOLD_MS = 100L
        private const val TAP_MS = 300L
    }
}

/** Wire constants shared with host/src/protocol.rs. */
object Proto {
    const val KIND_POINTER = 4
    const val KIND_SCROLL = 5
    const val POINTER_MOVE = 0
    const val POINTER_LEFT_DOWN = 1
    const val POINTER_DRAG = 2
    const val POINTER_LEFT_UP = 3
    const val POINTER_RIGHT_DOWN = 4
    const val POINTER_RIGHT_UP = 5
}
