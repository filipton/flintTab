package dev.tabdisplay

import android.content.Context
import android.provider.Settings
import android.view.Window
import android.view.WindowManager
import kotlin.math.exp
import kotlin.math.ln
import kotlin.math.sqrt

/**
 * The screen brightness the host sets (MSG_BRIGHTNESS), as a percentage on the same perceptual
 * scale as Android's own slider (its HLG curve), applied to this app's window only.
 */
object Brightness {
    private const val R = 0.5f
    private const val A = 0.17883277f
    private const val B = 0.28466892f
    private const val C = 0.55991073f

    /** Slider position (0..1) to the linear brightness the window takes. */
    private fun toLinear(p: Float): Float {
        val v = if (p <= R) (p / R) * (p / R) else exp((p - C) / A) + B
        return (v / 12f).coerceIn(0f, 1f)
    }

    private fun toSlider(linear: Float): Float {
        val v = linear * 12f
        return (if (v <= 1f) R * sqrt(v) else A * ln(v - B) + C).coerceIn(0f, 1f)
    }

    /** 0..100, or 255 for the tablet's own setting. */
    fun apply(window: Window, level: Int) {
        window.attributes = window.attributes.also {
            it.screenBrightness = if (level > 100) WindowManager.LayoutParams.BRIGHTNESS_OVERRIDE_NONE
                // Not 0: some panels turn the backlight off there.
                else maxOf(toLinear(level / 100f), 0.002f)
        }
    }

    /** The tablet's own setting, 0..100 on the slider's scale. */
    fun own(context: Context): Int = try {
        val v = Settings.System.getInt(context.contentResolver, Settings.System.SCREEN_BRIGHTNESS)
        Math.round(toSlider(v / 255f) * 100)
    } catch (_: Exception) { 50 }
}
