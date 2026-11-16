package org.inkbridge.android

import android.content.Context
import android.graphics.Bitmap
import android.graphics.Canvas
import android.graphics.Color
import android.graphics.Paint
import android.graphics.RectF
import android.view.MotionEvent
import android.view.View
import org.inkbridge.core.Finger
import org.inkbridge.core.FingerPhase
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

    /** Active multi-finger gesture: Android pointer id → wire finger id. */
    private val gesture = LinkedHashMap<Int, Int>()
    private var lastFrame: List<Finger> = emptyList()

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
            cancelGesture() // the pen wins over resting fingers
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
     * Fingers. Two or more fingers form a gesture that the computer receives
     * as native touch (pan/scroll with inertia, pinch-zoom). A single finger
     * or a resting palm does nothing, so writing is never disturbed.
     */
    private fun onFingers(ev: MotionEvent): Boolean {
        val action = ev.actionMasked
        val leaving = if (action == MotionEvent.ACTION_POINTER_UP || action == MotionEvent.ACTION_UP) ev.actionIndex else -1
        val down = (0 until ev.pointerCount).filter {
            ev.getToolType(it) == MotionEvent.TOOL_TYPE_FINGER && it != leaving
        }
        if (gesture.isEmpty()) {
            if (down.size >= 2 && action != MotionEvent.ACTION_CANCEL) {
                send(down.map { finger(ev, it, assignId(ev.getPointerId(it)), FingerPhase.DOWN) })
            }
            return true
        }
        when {
            action == MotionEvent.ACTION_CANCEL -> endGesture(ev, FingerPhase.CANCEL)
            down.size < 2 -> endGesture(ev, FingerPhase.UP)
            else -> {
                val frame = ArrayList<Finger>()
                for (i in down) {
                    val pid = ev.getPointerId(i)
                    val known = gesture[pid]
                    frame += if (known != null) finger(ev, i, known, FingerPhase.MOVE)
                    else finger(ev, i, assignId(pid), FingerPhase.DOWN)
                }
                if (leaving >= 0) gesture.remove(ev.getPointerId(leaving))?.let { id ->
                    frame += finger(ev, leaving, id, FingerPhase.UP)
                }
                send(frame)
            }
        }
        return true
    }

    /** Lifts (or cancels) every finger of the gesture at its last position. */
    private fun endGesture(ev: MotionEvent, phase: FingerPhase) {
        val frame = gesture.mapNotNull { (pid, id) ->
            val i = ev.findPointerIndex(pid)
            if (i >= 0) finger(ev, i, id, phase) else null
        }
        gesture.clear()
        send(frame)
        lastFrame = emptyList()
    }

    private fun assignId(pointerId: Int): Int {
        val id = (0 until 5).first { it !in gesture.values }
        gesture[pointerId] = id
        return id
    }

    private fun finger(ev: MotionEvent, i: Int, id: Int, phase: FingerPhase) = Finger(
        id = id.toUByte(),
        x = ((ev.getX(i) - area.left) / area.width()).coerceIn(0f, 1f),
        y = ((ev.getY(i) - area.top) / area.height()).coerceIn(0f, 1f),
        phase = phase,
    )

    private fun send(frame: List<Finger>) {
        if (frame.isEmpty() || area.width() <= 0f) return
        runCatching { session.sendTouch(frame) }
        lastFrame = frame.filter { it.phase == FingerPhase.DOWN || it.phase == FingerPhase.MOVE }
    }

    /** Cancels an active gesture at the fingers' last positions (no tap or click). */
    private fun cancelGesture() {
        if (gesture.isEmpty()) return
        gesture.clear()
        send(lastFrame.map { it.copy(phase = FingerPhase.CANCEL) })
        lastFrame = emptyList()
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
