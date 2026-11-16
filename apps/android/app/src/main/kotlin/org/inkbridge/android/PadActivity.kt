package org.inkbridge.android

import android.graphics.Color
import android.os.Bundle
import android.view.Gravity
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.WindowManager
import android.widget.Button
import android.widget.FrameLayout
import android.widget.TextView
import androidx.activity.ComponentActivity
import androidx.activity.OnBackPressedCallback
import androidx.core.view.WindowCompat
import androidx.core.view.WindowInsetsCompat
import androidx.core.view.WindowInsetsControllerCompat

/** Full-screen writing pad bound to the live session. */
class PadActivity : ComponentActivity() {

    private lateinit var info: TextView
    private lateinit var pad: PenPadView
    private var decoder: VideoDecoder? = null
    private val ticker = object : Runnable {
        override fun run() {
            val s = SessionHolder.session
            if (s == null || !s.isConnected()) {
                info.text = "Disconnected"
                info.setTextColor(Color.RED)
                return
            }
            val host = s.host()
            pad.onHostRegion(host.regionWidth, host.regionHeight)
            info.text = if (s.hasVideo()) {
                val l = s.latency()
                "%s · screen %.0f ms (p95 %.0f) · encode %.0f · net %.0f · %.0f fps".format(
                    host.name, l.totalMs, l.totalP95Ms, l.encodeMs, l.networkMs, l.fps,
                )
            } else {
                "${host.name} · ${"%.1f".format(s.rttUs().toDouble() / 1000.0)} ms"
            }
            // Fast enough that the area follows a rotation almost at once.
            info.postDelayed(this, 250)
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val session = SessionHolder.session ?: run { finish(); return }
        window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)

        pad = PenPadView(this, session)
        val screen = if (session.hasVideo()) SurfaceView(this) else null
        val ink = if (screen != null) InkLayer(this) else null
        pad.inkLayer = ink
        info = TextView(this).apply {
            textSize = 13f; alpha = 0.7f; setPadding(24, 12, 24, 12); setTextColor(Color.DKGRAY)
        }
        val clear = Button(this).apply {
            text = "Clear"
            alpha = 0.85f
            setOnClickListener { pad.clearInk() }
        }
        setContentView(FrameLayout(this).apply {
            setBackgroundColor(Color.BLACK)
            screen?.let { addView(it, FrameLayout.LayoutParams(0, 0)) }
            ink?.let { addView(it.view) }
            addView(pad)
            addView(clear, FrameLayout.LayoutParams(
                FrameLayout.LayoutParams.WRAP_CONTENT, FrameLayout.LayoutParams.WRAP_CONTENT, Gravity.TOP or Gravity.START,
            ))
            addView(info, FrameLayout.LayoutParams(
                FrameLayout.LayoutParams.WRAP_CONTENT, FrameLayout.LayoutParams.WRAP_CONTENT, Gravity.TOP or Gravity.END,
            ))
        })
        hideSystemBars()
        screen?.let { attachVideo(session, it) }

        onBackPressedDispatcher.addCallback(this, object : OnBackPressedCallback(true) {
            override fun handleOnBackPressed() {
                SessionHolder.close()
                finish()
            }
        })
    }

    /** The computer's screen, decoded under the transparent pen layer. */
    private fun attachVideo(session: org.inkbridge.core.Session, screen: SurfaceView) {
        pad.videoMode = true
        // Keep the picture exactly on the pen's active area.
        pad.onAreaChanged = { r ->
            screen.layoutParams = FrameLayout.LayoutParams(r.width().toInt(), r.height().toInt()).apply {
                leftMargin = r.left.toInt(); topMargin = r.top.toInt()
            }
        }
        screen.holder.addCallback(object : SurfaceHolder.Callback {
            override fun surfaceCreated(holder: SurfaceHolder) {
                decoder = VideoDecoder(session, holder.surface).also { it.start() }
            }
            override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) = Unit
            override fun surfaceDestroyed(holder: SurfaceHolder) {
                decoder?.stop(); decoder = null
            }
        })
    }

    override fun onStart() {
        super.onStart()
        info.post(ticker)
    }

    override fun onStop() {
        super.onStop()
        info.removeCallbacks(ticker)
    }

    private fun hideSystemBars() {
        WindowCompat.getInsetsController(window, window.decorView).apply {
            hide(WindowInsetsCompat.Type.systemBars())
            systemBarsBehavior = WindowInsetsControllerCompat.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE
        }
    }
}
