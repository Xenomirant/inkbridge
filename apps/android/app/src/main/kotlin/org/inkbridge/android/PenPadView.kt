package org.inkbridge.android

import android.content.Context
import android.graphics.Bitmap
import android.graphics.Canvas
import android.graphics.Color
import android.graphics.Paint
import android.graphics.RectF
import android.view.MotionEvent
import android.view.View
import androidx.input.motionprediction.MotionEventPredictor
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

        /** Local ink stays this long after a stroke, covering the round trip. */
        const val INK_FADE_MS = 700L

        /** Longest predicted extension drawn ahead of the pen tip. */
        const val MAX_PREDICTION_MM = 3f
    }

    /** Aspect of the desktop region currently mapped to this tablet. */
    private var hostAspect: Float = session.host().let { aspectOf(it.regionWidth, it.regionHeight) }
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
    /** Last centroid sent while scrolling (view pixels). */
    private var lastCx = 0f
    private var lastCy = 0f
    private val pxPerMm = resources.displayMetrics.xdpi / 25.4f

    /** Last state sent reliably; a change triggers a transition message. */
    private var lastContact: PenContact? = null
    private var lastEraser = false
    private var lastBarrel = false

    /** Android ends hover just before a pen-down; delay "out of range" briefly so it isn't sent for every stroke. */
    private val outOfRange = Runnable { emit(listOf(lastSampleAs(PenContact.OUT_OF_RANGE) ?: return@Runnable)) }
    private var lastSample: StylusSample? = null

    /** Called whenever the active (letterboxed) area moves or resizes. */
    var onAreaChanged: ((RectF) -> Unit)? = null

    /**
     * With video, the pad is a transparent layer over the computer's screen
     * and local ink is only a bridge until the real ink arrives: it fades
     * shortly after each stroke.
     */
    var videoMode = false
        set(v) {
            field = v
            setBackgroundColor(if (v) Color.TRANSPARENT else Color.rgb(250, 250, 250))
        }
    private val fadeInk = Runnable { clearInk() }

    init {
        setBackgroundColor(Color.rgb(250, 250, 250))
        isFocusable = true
    }

    /** Front-buffered ink over the video (video mode only). */
    var inkLayer: InkLayer? = null

    /** Predicts where the pen tip will be next, to draw slightly ahead of it. */
    private val predictor: MotionEventPredictor by lazy { MotionEventPredictor.newInstance(this) }
    /** The prediction currently on screen, erased before the next one is drawn. */
    private var shownPrediction: InkLayer.Segment? = null

    fun clearInk() {
        ink?.eraseColor(Color.TRANSPARENT); invalidate()
        inkLayer?.clear()
    }

    override fun onSizeChanged(w: Int, h: Int, oldw: Int, oldh: Int) {
        if (w <= 0 || h <= 0) return
        layoutArea()
        ink = Bitmap.createBitmap(w, h, Bitmap.Config.ARGB_8888).also { inkCanvas = Canvas(it) }
        // Rotation or resize: let the computer pick the matching desktop region.
        runCatching { session.sendViewport(w.toUInt(), h.toUInt()) }
    }

    /** Called when the computer maps a different desktop region. */
    fun onHostRegion(width: UInt, height: UInt) {
        val aspect = aspectOf(width, height)
        if (kotlin.math.abs(aspect - hostAspect) < 1e-3f) return
        hostAspect = aspect
        layoutArea()
        clearInk()
    }

    /** The largest area with the region's aspect, centred (letterboxed). */
    private fun layoutArea() {
        val w = width.toFloat(); val h = height.toFloat()
        if (w <= 0f || h <= 0f) return
        if (w / h > hostAspect) {
            val aw = h * hostAspect; area.set((w - aw) / 2, 0f, (w + aw) / 2, h)
        } else {
            val ah = w / hostAspect; area.set(0f, (h - ah) / 2, w, (h + ah) / 2)
        }
        onAreaChanged?.invoke(RectF(area))
        invalidate()
    }

    private fun aspectOf(w: UInt, h: UInt): Float =
        if (w == 0u || h == 0u) 16f / 9f else w.toFloat() / h.toFloat()

    override fun onDraw(canvas: Canvas) {
        canvas.drawRect(area, framePaint)
        ink?.let { canvas.drawBitmap(it, 0f, 0f, null) }
    }

    override fun onTouchEvent(ev: MotionEvent): Boolean {
        val i = StylusMapper.stylusIndex(ev)
        if (i < 0) return onFingers(ev)
        if (ev.actionMasked == MotionEvent.ACTION_DOWN) {
            removeCallbacks(fadeInk)
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
        if (inkLayer != null) predictor.record(ev)
        drawLocal(ev, i, contact)
        drawPrediction(ev, i, contact)
        if (videoMode && contact != PenContact.CONTACT) postDelayed(fadeInk, INK_FADE_MS)
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
            if (mode == Mode.ZOOM) {
                send(listOf(wire(0, startAx, startAy, FingerPhase.DOWN), wire(1, startBx, startBy, FingerPhase.DOWN)))
            } else {
                // Scrolling starts from where the fingers first touched.
                lastCx = (startAx + startBx) / 2; lastCy = (startAy + startBy) / 2
            }
        }
        if (mode == Mode.PAN) {
            // Touchpad-style scroll: the centroid's movement, with the event
            // time so the computer can compute speed (acceleration, inertia).
            val cx = (ax + bx) / 2; val cy = (ay + by) / 2
            sendScroll(cx, cy, cx - lastCx, cy - lastCy, StylusMapper.timestampUs(ev, -1), end = false)
            lastCx = cx; lastCy = cy
        } else {
            send(listOf(wire(0, ax, ay, FingerPhase.MOVE), wire(1, bx, by, FingerPhase.MOVE)))
        }
        return true
    }

    private fun sendScroll(cx: Float, cy: Float, dx: Float, dy: Float, tUs: Long, end: Boolean) {
        if (area.width() <= 0f) return
        runCatching {
            session.sendScroll(
                ((cx - area.left) / area.width()).coerceIn(0f, 1f),
                ((cy - area.top) / area.height()).coerceIn(0f, 1f),
                dx / area.width(), dy / area.height(), tUs.toULong(), end,
            )
        }
    }

    /** Lifts (or cancels) both fingers where they were last sent. */
    private fun endGesture(phase: FingerPhase) {
        when (mode) {
            // Lift: the computer decides whether to coast (inertia). A cancel
            // (pen touched down) is a lift too; the pen stops any inertia.
            Mode.PAN -> sendScroll(lastCx, lastCy, 0f, 0f, android.os.SystemClock.uptimeMillis() * 1000, end = true)
            Mode.ZOOM -> send(lastFrame.map { it.copy(phase = phase) })
            else -> {}
        }
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

    /**
     * Draws a short predicted extension beyond the last real point (video
     * mode only). The previous prediction is erased first; the real segment
     * drawn just before repairs any ink the erasing touched.
     */
    private fun drawPrediction(ev: MotionEvent, i: Int, contact: PenContact) {
        val front = inkLayer ?: return
        shownPrediction?.let { front.draw(it.copy(width = it.width + 2f, erase = true)) }
        shownPrediction = null
        if (contact != PenContact.CONTACT || lastX.isNaN() || ev.getToolType(i) == MotionEvent.TOOL_TYPE_ERASER) return
        val predicted = predictor.predict() ?: return
        try {
            val pi = predicted.findPointerIndex(ev.getPointerId(i))
            if (pi < 0) return
            var dx = predicted.getX(pi) - lastX
            var dy = predicted.getY(pi) - lastY
            val len = kotlin.math.hypot(dx, dy)
            if (len < 0.5f) return
            // Cap the guess: a sudden turn must never fling ink away.
            val max = MAX_PREDICTION_MM * resources.displayMetrics.xdpi / 25.4f
            if (len > max) { dx *= max / len; dy *= max / len }
            val w = 1.5f + ev.getPressure(i).coerceIn(0f, 1f) * 9f
            val seg = InkLayer.Segment(lastX, lastY, lastX + dx, lastY + dy, w, erase = false)
            front.draw(seg)
            shownPrediction = seg
        } finally {
            predicted.recycle()
        }
    }

    private fun drawLocal(ev: MotionEvent, i: Int, contact: PenContact) {
        if (contact != PenContact.CONTACT) { lastX = Float.NaN; return }
        val front = inkLayer
        val c = inkCanvas
        if (front == null && c == null) return
        val eraser = ev.getToolType(i) == MotionEvent.TOOL_TYPE_ERASER
        inkPaint.color = if (eraser) Color.rgb(250, 250, 250) else Color.rgb(60, 120, 255)
        for (h in 0..ev.historySize) {
            val x = if (h < ev.historySize) ev.getHistoricalX(i, h) else ev.getX(i)
            val y = if (h < ev.historySize) ev.getHistoricalY(i, h) else ev.getY(i)
            val p = if (h < ev.historySize) ev.getHistoricalPressure(i, h) else ev.getPressure(i)
            if (!lastX.isNaN()) {
                val w = if (eraser) 40f else 1.5f + p.coerceIn(0f, 1f) * 9f
                if (front != null) {
                    // Video mode: straight into the front buffer (next refresh).
                    front.draw(InkLayer.Segment(lastX, lastY, x, y, w, eraser))
                } else {
                    inkPaint.strokeWidth = w
                    c?.drawLine(lastX, lastY, x, y, inkPaint)
                }
            }
            lastX = x; lastY = y
        }
        if (front == null) invalidate()
    }
}
