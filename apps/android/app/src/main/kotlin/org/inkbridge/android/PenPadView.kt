package org.inkbridge.android

import android.content.Context
import android.graphics.Bitmap
import android.graphics.Canvas
import android.graphics.Color
import android.graphics.Paint
import android.graphics.RectF
import android.view.MotionEvent
import android.view.View
import org.inkbridge.core.PenContact
import org.inkbridge.core.Session
import org.inkbridge.core.StylusSample

/**
 * Full-screen writing surface. Captures the stylus at full sample rate,
 * sends every sample to the computer immediately, and draws local ink so
 * writing feels instant (M2 replaces this with front-buffered rendering and
 * the streamed desktop underneath).
 *
 * The active area keeps the computer screen's aspect ratio (letterboxed), so
 * strokes are never stretched.
 */
class PenPadView(context: Context, private val session: Session) : View(context) {

    private val hostAspect: Float = session.host().let {
        if (it.regionHeight == 0u) 16f / 9f else it.regionWidth.toFloat() / it.regionHeight.toFloat()
    }
    private val area = RectF()
    private var ink: Bitmap? = null
    private var inkCanvas: Canvas? = null
    private val inkPaint = Paint(Paint.ANTI_ALIAS_FLAG).apply {
        color = Color.rgb(60, 120, 255); strokeCap = Paint.Cap.ROUND; style = Paint.Style.STROKE
    }
    private val framePaint = Paint().apply { color = Color.argb(90, 128, 128, 128); style = Paint.Style.STROKE; strokeWidth = 3f }
    private var lastX = Float.NaN
    private var lastY = Float.NaN

    /** Two-finger scroll state: previous centroid, or NaN when not scrolling. */
    private var scrollX = Float.NaN
    private var scrollY = Float.NaN

    /** Last state sent reliably; a change triggers a transition message. */
    private var lastContact: PenContact? = null
    private var lastEraser = false
    private var lastBarrel = false

    /** Android ends hover just before a pen-down; delay "out of range" briefly so it isn't sent for every stroke. */
    private val outOfRange = Runnable { emit(listOf(lastSampleAs(PenContact.OUT_OF_RANGE) ?: return@Runnable)) }
    private var lastSample: StylusSample? = null

    init {
        setBackgroundColor(Color.rgb(250, 250, 250))
        isFocusable = true
    }

    fun clearInk() {
        ink?.eraseColor(Color.TRANSPARENT); invalidate()
    }

    override fun onSizeChanged(w: Int, h: Int, oldw: Int, oldh: Int) {
        val viewAspect = w.toFloat() / h
        if (viewAspect > hostAspect) {
            val aw = h * hostAspect; area.set((w - aw) / 2, 0f, (w + aw) / 2, h.toFloat())
        } else {
            val ah = w / hostAspect; area.set(0f, (h - ah) / 2, w.toFloat(), (h + ah) / 2)
        }
        ink = Bitmap.createBitmap(w, h, Bitmap.Config.ARGB_8888).also { inkCanvas = Canvas(it) }
    }

    override fun onDraw(canvas: Canvas) {
        canvas.drawRect(area, framePaint)
        ink?.let { canvas.drawBitmap(it, 0f, 0f, null) }
    }

    override fun onTouchEvent(ev: MotionEvent): Boolean {
        val i = StylusMapper.stylusIndex(ev)
        if (i < 0) return onFingers(ev)
        if (ev.actionMasked == MotionEvent.ACTION_DOWN) {
            removeCallbacks(outOfRange)
            requestUnbufferedDispatch(ev) // full sample rate, no 60 Hz batching
        }
        val contact = when (ev.actionMasked) {
            MotionEvent.ACTION_DOWN, MotionEvent.ACTION_MOVE -> PenContact.CONTACT
            MotionEvent.ACTION_UP -> PenContact.HOVER
            MotionEvent.ACTION_CANCEL -> PenContact.OUT_OF_RANGE
            else -> return true
        }
        val samples = StylusMapper.samples(ev, i, area, contact)
        emit(samples)
        drawLocal(ev, i, contact)
        return true
    }

    /**
     * Fingers: two-finger drag scrolls the computer; a single finger or a
     * resting palm does nothing (palm rejection while writing).
     */
    private fun onFingers(ev: MotionEvent): Boolean {
        val fingers = (0 until ev.pointerCount).filter { ev.getToolType(it) == MotionEvent.TOOL_TYPE_FINGER }
        val lifting = ev.actionMasked == MotionEvent.ACTION_POINTER_UP
        val active = if (lifting) fingers.filter { it != ev.actionIndex } else fingers
        if (active.size != 2 || ev.actionMasked == MotionEvent.ACTION_UP || ev.actionMasked == MotionEvent.ACTION_CANCEL) {
            scrollX = Float.NaN; scrollY = Float.NaN
            return true
        }
        val cx = active.map { ev.getX(it) }.average().toFloat()
        val cy = active.map { ev.getY(it) }.average().toFloat()
        if (!scrollX.isNaN() && ev.actionMasked == MotionEvent.ACTION_MOVE && area.width() > 0f) {
            val dx = (cx - scrollX) / area.width()
            val dy = (cy - scrollY) / area.height()
            if (dx != 0f || dy != 0f) runCatching { session.sendScroll(dx, dy) }
        }
        scrollX = cx; scrollY = cy
        return true
    }

    override fun onHoverEvent(ev: MotionEvent): Boolean {
        val i = StylusMapper.stylusIndex(ev)
        if (i < 0) return false
        when (ev.actionMasked) {
            MotionEvent.ACTION_HOVER_ENTER, MotionEvent.ACTION_HOVER_MOVE -> {
                removeCallbacks(outOfRange)
                emit(StylusMapper.samples(ev, i, area, PenContact.HOVER))
            }
            MotionEvent.ACTION_HOVER_EXIT -> postDelayed(outOfRange, 60)
        }
        return true
    }

    private fun emit(samples: List<StylusSample>) {
        if (samples.isEmpty()) return
        runCatching { session.sendPen(samples) }
        val last = samples.last()
        lastSample = last
        if (last.contact != lastContact || last.eraser != lastEraser || last.barrelButton != lastBarrel) {
            runCatching { session.sendTransition(last) }
            lastContact = last.contact; lastEraser = last.eraser; lastBarrel = last.barrelButton
        }
    }

    private fun lastSampleAs(contact: PenContact): StylusSample? =
        lastSample?.copy(contact = contact, pressure = 0f, tUs = lastSample!!.tUs + 1u)

    private fun drawLocal(ev: MotionEvent, i: Int, contact: PenContact) {
        val c = inkCanvas ?: return
        if (contact != PenContact.CONTACT) { lastX = Float.NaN; return }
        val eraser = ev.getToolType(i) == MotionEvent.TOOL_TYPE_ERASER
        inkPaint.color = if (eraser) Color.rgb(250, 250, 250) else Color.rgb(60, 120, 255)
        for (h in 0..ev.historySize) {
            val x = if (h < ev.historySize) ev.getHistoricalX(i, h) else ev.getX(i)
            val y = if (h < ev.historySize) ev.getHistoricalY(i, h) else ev.getY(i)
            val p = if (h < ev.historySize) ev.getHistoricalPressure(i, h) else ev.getPressure(i)
            if (!lastX.isNaN()) {
                inkPaint.strokeWidth = if (eraser) 40f else 1.5f + p.coerceIn(0f, 1f) * 9f
                c.drawLine(lastX, lastY, x, y, inkPaint)
            }
            lastX = x; lastY = y
        }
        invalidate()
    }
}
