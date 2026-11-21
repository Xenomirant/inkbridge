package org.inkbridge.android

import android.graphics.RectF
import android.os.Build
import android.view.MotionEvent
import org.inkbridge.core.PenContact
import org.inkbridge.core.StylusSample
import kotlin.math.atan
import kotlin.math.cos
import kotlin.math.min
import kotlin.math.sin
import kotlin.math.tan

/**
 * Converts Android stylus events into protocol samples.
 *
 * Android reports tilt as a polar pair (AXIS_TILT: angle from vertical,
 * AXIS_ORIENTATION: direction the pen top points). The protocol, like
 * Windows and the W3C Pointer Events spec, uses Cartesian tilt per axis.
 */
object StylusMapper {

    fun isStylus(ev: MotionEvent, index: Int): Boolean {
        val t = ev.getToolType(index)
        return t == MotionEvent.TOOL_TYPE_STYLUS || t == MotionEvent.TOOL_TYPE_ERASER
    }

    /** Index of the first stylus pointer, or -1 (fingers are ignored: palm rejection). */
    fun stylusIndex(ev: MotionEvent): Int =
        (0 until ev.pointerCount).firstOrNull { isStylus(ev, it) } ?: -1

    /**
     * All samples carried by [ev] for pointer [index]: the historical
     * (batched) ones first, then the current one. Keeps the pen's full
     * hardware sample rate instead of one sample per frame.
     */
    fun samples(ev: MotionEvent, index: Int, area: RectF, contact: PenContact): List<StylusSample> {
        val out = ArrayList<StylusSample>(ev.historySize + 1)
        for (h in 0 until ev.historySize) out += sample(ev, index, h, area, contact)
        out += sample(ev, index, -1, area, contact)
        return out
    }

    /** One sample; `pos = -1` means the current (non-historical) values. */
    fun sample(ev: MotionEvent, index: Int, pos: Int, area: RectF, contact: PenContact): StylusSample {
        fun axis(a: Int) = if (pos < 0) ev.getAxisValue(a, index) else ev.getHistoricalAxisValue(a, index, pos)
        val x = if (pos < 0) ev.getX(index) else ev.getHistoricalX(index, pos)
        val y = if (pos < 0) ev.getY(index) else ev.getHistoricalY(index, pos)
        val pressure = if (pos < 0) ev.getPressure(index) else ev.getHistoricalPressure(index, pos)
        val (tiltX, tiltY) = cartesianTilt(axis(MotionEvent.AXIS_TILT), axis(MotionEvent.AXIS_ORIENTATION))
        return StylusSample(
            tUs = timestampUs(ev, pos).toULong(),
            x = normalise(x, area.left, area.width()),
            y = normalise(y, area.top, area.height()),
            pressure = if (contact == PenContact.CONTACT) pressure.coerceIn(0f, 1f) else 0f,
            tiltX = tiltX,
            tiltY = tiltY,
            rotation = 0f,
            contact = contact,
            eraser = ev.getToolType(index) == MotionEvent.TOOL_TYPE_ERASER,
            barrelButton = isBarrelPressed(ev.buttonState),
        )
    }

    /**
     * Whether a pen button is held. Pens differ in which button they report: the Tab Pen Plus
     * sends BTN_STYLUS2 (BUTTON_STYLUS_SECONDARY); some report the mouse-style BUTTON_SECONDARY.
     * The protocol has one barrel button, so any of them counts.
     */
    fun isBarrelPressed(buttonState: Int): Boolean =
        buttonState and BARREL_BUTTONS != 0

    private const val BARREL_BUTTONS =
        MotionEvent.BUTTON_STYLUS_PRIMARY or MotionEvent.BUTTON_STYLUS_SECONDARY or MotionEvent.BUTTON_SECONDARY

    /** Polar (tilt from vertical, orientation) in radians → per-axis degrees. */
    fun cartesianTilt(tilt: Float, orientation: Float): Pair<Float, Float> {
        val t = tan(min(tilt.toDouble(), 1.5)) // avoid tan(π/2)
        val tx = Math.toDegrees(atan(sin(orientation.toDouble()) * t)).toFloat()
        // Orientation 0 = pen top points up the screen, i.e. leaning away from the user (−y).
        val ty = Math.toDegrees(atan(-cos(orientation.toDouble()) * t)).toFloat()
        return tx to ty
    }

    private fun normalise(v: Float, origin: Float, size: Float): Float =
        if (size <= 0f) 0f else ((v - origin) / size).coerceIn(0f, 1f)

    /** Event time in µs (the same monotonic clock as `SystemClock.uptimeMillis`). */
    fun timestampUs(ev: MotionEvent, pos: Int): Long =
        if (Build.VERSION.SDK_INT >= 34) {
            (if (pos < 0) ev.eventTimeNanos else ev.getHistoricalEventTimeNanos(pos)) / 1_000
        } else {
            (if (pos < 0) ev.eventTime else ev.getHistoricalEventTime(pos)) * 1_000
        }
}
