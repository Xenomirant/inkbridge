package org.inkbridge.android

import android.content.Context
import android.graphics.Bitmap
import android.graphics.Canvas
import android.graphics.Color
import android.graphics.Paint
import android.graphics.RectF
import android.view.MotionEvent
import android.view.View
import kotlin.math.abs
import kotlin.math.hypot
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

    private companion object {
        /** Movement before a two-finger gesture is classified (libinput uses a few mm). */
        const val GESTURE_SLOP_MM = 4f
    }

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

    /**
     * Two-finger gesture, recognised like touchpad drivers do (libinput):
     * nothing is sent until the motion is unambiguous (> [GESTURE_SLOP_MM]),
     * then the gesture is locked as PAN or ZOOM until the fingers lift.
     */
    private enum class Mode { NONE, UNDECIDED, PAN, ZOOM }
    private var mode = Mode.NONE
    private var pidA = -1
    private var pidB = -1
    private var startAx = 0f; private var startAy = 0f
    private var startBx = 0f; private var startBy = 0f
    private var lastFrame: List<Finger> = emptyList()
    private val pxPerMm = resources.displayMetrics.xdpi / 25.4f

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
     * Fingers. Two fingers form a gesture the computer receives as native
     * touch: a PAN is sent as two fingers at a fixed distance (so the OS can
     * never mistake it for zooming), a ZOOM as the real finger positions.
     * A single finger or a resting palm does nothing.
     */
    private fun onFingers(ev: MotionEvent): Boolean {
        val action = ev.actionMasked
        val leaving = if (action == MotionEvent.ACTION_POINTER_UP || action == MotionEvent.ACTION_UP) ev.actionIndex else -1
        val down = (0 until ev.pointerCount).filter {
            ev.getToolType(it) == MotionEvent.TOOL_TYPE_FINGER && it != leaving
        }
        if (mode == Mode.NONE) {
            if (down.size >= 2 && action != MotionEvent.ACTION_CANCEL) {
                pidA = ev.getPointerId(down[0]); pidB = ev.getPointerId(down[1])
                startAx = ev.getX(down[0]); startAy = ev.getY(down[0])
                startBx = ev.getX(down[1]); startBy = ev.getY(down[1])
                mode = Mode.UNDECIDED
            }
            return true
        }
        val ia = ev.findPointerIndex(pidA)
        val ib = ev.findPointerIndex(pidB)
        val lost = ia < 0 || ib < 0 || ia == leaving || ib == leaving
        if (action == MotionEvent.ACTION_CANCEL) { endGesture(FingerPhase.CANCEL); return true }
        if (lost) { endGesture(FingerPhase.UP); return true }

        val ax = ev.getX(ia); val ay = ev.getY(ia); val bx = ev.getX(ib); val by = ev.getY(ib)
        if (mode == Mode.UNDECIDED) {
            val translation = hypot((ax + bx - startAx - startBx) / 2, (ay + by - startAy - startBy) / 2)
            val spreadChange = abs(hypot(bx - ax, by - ay) - hypot(startBx - startAx, startBy - startAy))
            if (maxOf(translation, spreadChange) < GESTURE_SLOP_MM * pxPerMm) return true
            mode = if (spreadChange > translation) Mode.ZOOM else Mode.PAN
            send(listOf(wire(0, startAx, startAy, FingerPhase.DOWN), wire(1, startBx, startBy, FingerPhase.DOWN)))
        }
        val frame = if (mode == Mode.PAN) {
            // Keep the start separation: only the centroid moves.
            val dcx = (ax + bx - startAx - startBx) / 2
            val dcy = (ay + by - startAy - startBy) / 2
            listOf(wire(0, startAx + dcx, startAy + dcy, FingerPhase.MOVE), wire(1, startBx + dcx, startBy + dcy, FingerPhase.MOVE))
        } else {
            listOf(wire(0, ax, ay, FingerPhase.MOVE), wire(1, bx, by, FingerPhase.MOVE))
        }
        send(frame)
        return true
    }

    /** Lifts (or cancels) both fingers where they were last sent. */
    private fun endGesture(phase: FingerPhase) {
        if (mode == Mode.PAN || mode == Mode.ZOOM) send(lastFrame.map { it.copy(phase = phase) })
        mode = Mode.NONE
        lastFrame = emptyList()
    }

    /** Cancels an active gesture (no tap or click), e.g. when the pen touches down. */
    private fun cancelGesture() = endGesture(FingerPhase.CANCEL)

    private fun wire(id: Int, x: Float, y: Float, phase: FingerPhase) = Finger(
        id = id.toUByte(),
        x = ((x - area.left) / area.width()).coerceIn(0f, 1f),
        y = ((y - area.top) / area.height()).coerceIn(0f, 1f),
        phase = phase,
    )

    private fun send(frame: List<Finger>) {
        if (frame.isEmpty() || area.width() <= 0f) return
        runCatching { session.sendTouch(frame) }
        lastFrame = frame.filter { it.phase == FingerPhase.DOWN || it.phase == FingerPhase.MOVE }
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
