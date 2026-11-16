package org.inkbridge.android

import android.content.Context
import android.graphics.BlendMode
import android.graphics.Canvas
import android.graphics.Color
import android.graphics.Paint
import android.graphics.PixelFormat
import android.view.SurfaceHolder
import android.view.SurfaceView
import androidx.graphics.lowlatency.CanvasFrontBufferedRenderer

/**
 * Lowest-latency local ink, drawn over the streamed screen.
 *
 * Uses front-buffered rendering (androidx.graphics): each pen segment is
 * drawn straight into the buffer the display is scanning out, so it appears
 * on the next refresh instead of after 2–3 frames of buffering. The ink is a
 * bridge only: it's cleared shortly after writing pauses, when the
 * computer's real stroke has arrived in the video underneath. Nothing is
 * ever committed to the multi-buffered layer.
 */
class InkLayer(context: Context) {

    /** One straight piece of a stroke, in view pixels. */
    data class Segment(val x0: Float, val y0: Float, val x1: Float, val y1: Float, val width: Float, val erase: Boolean)

    val view = SurfaceView(context).apply {
        // Above the video surface, below the window (where the pen input view lives).
        setZOrderMediaOverlay(true)
        holder.setFormat(PixelFormat.TRANSLUCENT)
    }

    private var renderer: CanvasFrontBufferedRenderer<Segment>? = null

    private val ink = Paint(Paint.ANTI_ALIAS_FLAG).apply {
        color = Color.rgb(60, 120, 255); strokeCap = Paint.Cap.ROUND; style = Paint.Style.STROKE
    }
    private val eraser = Paint(Paint.ANTI_ALIAS_FLAG).apply {
        blendMode = BlendMode.CLEAR; strokeCap = Paint.Cap.ROUND; style = Paint.Style.STROKE
    }

    private val callback = object : CanvasFrontBufferedRenderer.Callback<Segment> {
        override fun onDrawFrontBufferedLayer(canvas: Canvas, bufferWidth: Int, bufferHeight: Int, param: Segment) {
            val p = if (param.erase) eraser else ink
            p.strokeWidth = param.width
            canvas.drawLine(param.x0, param.y0, param.x1, param.y1, p)
        }

        override fun onDrawMultiBufferedLayer(canvas: Canvas, bufferWidth: Int, bufferHeight: Int, params: Collection<Segment>) {
            canvas.drawColor(Color.TRANSPARENT, android.graphics.PorterDuff.Mode.CLEAR)
        }
    }

    init {
        view.holder.addCallback(object : SurfaceHolder.Callback {
            override fun surfaceCreated(holder: SurfaceHolder) {
                renderer = CanvasFrontBufferedRenderer(view, callback)
            }
            override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) = Unit
            override fun surfaceDestroyed(holder: SurfaceHolder) {
                renderer?.release(true); renderer = null
            }
        })
    }

    fun draw(segment: Segment) {
        renderer?.renderFrontBufferedLayer(segment)
    }

    fun clear() {
        renderer?.clear()
    }
}
