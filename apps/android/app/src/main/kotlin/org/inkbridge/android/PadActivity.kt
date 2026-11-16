package org.inkbridge.android

import android.graphics.Color
import android.os.Bundle
import android.view.Gravity
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
            info.text = "${host.name} · ${"%.1f".format(s.rttUs().toDouble() / 1000.0)} ms"
            // Fast enough that the area follows a rotation almost at once.
            info.postDelayed(this, 250)
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val session = SessionHolder.session ?: run { finish(); return }
        window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)

        pad = PenPadView(this, session)
        info = TextView(this).apply {
            textSize = 13f; alpha = 0.7f; setPadding(24, 12, 24, 12); setTextColor(Color.DKGRAY)
        }
        val clear = Button(this).apply {
            text = "Clear"
            alpha = 0.85f
            setOnClickListener { pad.clearInk() }
        }
        setContentView(FrameLayout(this).apply {
            addView(pad)
            addView(clear, FrameLayout.LayoutParams(
                FrameLayout.LayoutParams.WRAP_CONTENT, FrameLayout.LayoutParams.WRAP_CONTENT, Gravity.TOP or Gravity.START,
            ))
            addView(info, FrameLayout.LayoutParams(
                FrameLayout.LayoutParams.WRAP_CONTENT, FrameLayout.LayoutParams.WRAP_CONTENT, Gravity.TOP or Gravity.END,
            ))
        })
        hideSystemBars()

        onBackPressedDispatcher.addCallback(this, object : OnBackPressedCallback(true) {
            override fun handleOnBackPressed() {
                SessionHolder.close()
                finish()
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
