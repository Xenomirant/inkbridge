package org.inkbridge.android

import android.content.Context
import android.graphics.Color
import android.net.wifi.WifiManager
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
import kotlin.concurrent.thread

/** Full-screen writing pad bound to the live session. */
class PadActivity : ComponentActivity() {

    private lateinit var info: TextView
    private lateinit var pad: PenPadView
    private var decoder: VideoDecoder? = null

    /**
     * The session this screen was built around. Its views and decoder use
     * it until they are gone, so it is freed in [onDestroy], not earlier.
     */
    private var own: org.inkbridge.core.Session? = null

    /**
     * Stops the Wi-Fi radio from sleeping between packets while the pad is
     * on screen. Without it the first frames after a pause arrive up to a
     * few hundred milliseconds late.
     */
    private val wifiLock by lazy {
        (applicationContext.getSystemService(Context.WIFI_SERVICE) as WifiManager)
            .createWifiLock(WifiManager.WIFI_MODE_FULL_LOW_LATENCY, "inkbridge:pad")
            .apply { setReferenceCounted(false) }
    }
    /** Opens the display menu; shown only when the computer has several. */
    private var screens: Button? = null
    private var ticks = 0
    private val ticker = object : Runnable {
        override fun run() {
            val s = SessionHolder.session
            if (s == null) {
                info.text = "Disconnected"
                info.setTextColor(Color.RED)
                return
            }
            if (!s.isConnected()) {
                info.text = "Connection lost, reconnecting…"
                info.setTextColor(Color.RED)
                reconnect()
                return
            }
            val host = s.host()
            screens?.visibility = if (s.displays().size > 1) android.view.View.VISIBLE else android.view.View.GONE
            pad.onHostRegion(host.regionX, host.regionY, host.regionWidth, host.regionHeight)
            info.text = if (s.hasVideo()) {
                val l = s.latency()
                "%s · %s · screen %.0f ms (p95 %.0f) · encode %.0f · net %.0f · %.0f fps · %.1f Mbit/s".format(
                    host.name, if (host.usb) "USB" else "Wi-Fi",
                    l.totalMs, l.totalP95Ms, l.encodeMs, l.networkMs, l.fps, l.bitrateMbps,
                )
            } else {
                "${host.name} · ${"%.1f".format(s.rttUs().toDouble() / 1000.0)} ms"
            }
            if (++ticks % 8 == 0) android.util.Log.d("Inkbridge", info.text.toString().substringAfter(" · "))
            // Fast enough that the area follows a rotation almost at once.
            info.postDelayed(this, 250)
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val session = SessionHolder.session ?: run { finish(); return }
        own = session
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
        // Zoomable view: "View" switches two-finger gestures to moving and
        // zooming the tablet's view of the desktop; "Fit" shows it all.
        val view = Button(this).apply {
            text = "View"
            alpha = 0.85f
            setOnClickListener {
                pad.viewMode = !pad.viewMode
                text = if (pad.viewMode) "View ✓" else "View"
            }
        }
        val fit = Button(this).apply {
            text = "Fit"
            alpha = 0.85f
            setOnClickListener { pad.fitWholeDisplay() }
        }
        // Which of the computer's displays to show and draw on.
        val choose = Button(this).apply {
            text = "Screen"
            alpha = 0.85f
            visibility = android.view.View.GONE
            setOnClickListener { chooseDisplay(session) }
        }
        screens = choose
        val tools = android.widget.LinearLayout(this).apply {
            orientation = android.widget.LinearLayout.HORIZONTAL
            addView(clear); if (screen != null) { addView(view); addView(fit); addView(choose) }
        }
        setContentView(FrameLayout(this).apply {
            setBackgroundColor(Color.BLACK)
            screen?.let { addView(it, FrameLayout.LayoutParams(0, 0)) }
            ink?.let { addView(it.view) }
            addView(pad)
            addView(tools, FrameLayout.LayoutParams(
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
                SessionHolder.session = null
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
        // View gesture preview: stretch and move the current picture so the
        // selected part of the desktop fills the area, until the computer
        // switches to capturing exactly that part.
        pad.onViewPreview = { draft ->
            val area = pad.activeArea()
            val shown = pad.shownRegion()
            if (draft.width() > 0f && shown.width() > 0f) {
                val s = area.width() / draft.width() // screen px per desktop px
                screen.layoutParams = FrameLayout.LayoutParams(
                    (shown.width() * s).toInt(), (shown.height() * s).toInt(),
                ).apply {
                    leftMargin = (area.left + (shown.left - draft.left) * s).toInt()
                    topMargin = (area.top + (shown.top - draft.top) * s).toInt()
                }
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

    private fun chooseDisplay(session: org.inkbridge.core.Session) {
        val displays = session.displays()
        if (displays.size < 2) return
        // An offer (to become an extra screen) has no size yet.
        val names = displays.map {
            if (it.width == 0u) it.name else "${it.name} · ${it.width}×${it.height}"
        }.toTypedArray()
        android.app.AlertDialog.Builder(this)
            .setTitle("Show which screen?")
            .setSingleChoiceItems(names, displays.indexOfFirst { it.current }) { dialog, which ->
                if (!displays[which].current) {
                    pad.clearInk()
                    runCatching { session.selectDisplay(which.toUInt()) }
                }
                dialog.dismiss()
            }
            .setNegativeButton("Cancel", null)
            .setOnDismissListener { hideSystemBars() }
            .show()
    }

    @Volatile private var reconnecting = false

    /**
     * The connection dropped (the computer's app restarted, Wi-Fi blinked):
     * keep trying to get it back while the pad is on screen, then rebuild
     * the pad around the new session. Over the USB cable link the computer
     * dials the tablet; otherwise the tablet dials the computer.
     */
    private fun reconnect() {
        if (reconnecting) return
        val code = SessionHolder.code ?: return
        val dead = SessionHolder.session ?: return
        val identity = runCatching { Storage(this).identity() }.getOrNull() ?: return
        val metrics = resources.displayMetrics
        reconnecting = true
        thread(name = "inkbridge-reconnect") {
            var fresh: org.inkbridge.core.Session? = null
            while (reconnecting && fresh == null) {
                val screen = VideoDecoder.screen(metrics.widthPixels, metrics.heightPixels)
                fresh = Reconnect.attempt(
                    UsbLink.tetherAddress(),
                    { ip ->
                        runCatching {
                            org.inkbridge.core.Session.acceptUsb(code, identity, deviceName(), ip, USB_WAIT_MS, screen)
                        }.getOrNull()
                    },
                    {
                        runCatching {
                            org.inkbridge.core.Session.connect(code, identity, deviceName(), false, screen)
                        }.getOrNull()
                    },
                )
                if (fresh == null) Thread.sleep(RETRY_MS)
            }
            val session = fresh ?: return@thread
            runOnUiThread {
                // Only if the pad is still showing the session that dropped.
                if (reconnecting && SessionHolder.session === dead) {
                    SessionHolder.session = session
                    recreate()
                } else {
                    SessionHolder.release(session)
                }
                reconnecting = false
            }
        }
    }

    override fun onDestroy() {
        super.onDestroy()
        decoder?.stop(); decoder = null
        // Left by Back, or replaced after a reconnect.
        own?.let { if (SessionHolder.session !== it) SessionHolder.release(it) }
        own = null
    }

    override fun onStart() {
        super.onStart()
        runCatching { wifiLock.acquire() }
        info.post(ticker)
    }

    override fun onStop() {
        super.onStop()
        runCatching { wifiLock.release() }
        info.removeCallbacks(ticker)
        reconnecting = false
    }

    private companion object {
        const val RETRY_MS = 1000L

        /** How long one round waits for the computer to dial over USB. */
        const val USB_WAIT_MS = 2000u
    }

    private fun hideSystemBars() {
        WindowCompat.getInsetsController(window, window.decorView).apply {
            hide(WindowInsetsCompat.Type.systemBars())
            systemBarsBehavior = WindowInsetsControllerCompat.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE
        }
    }
}
