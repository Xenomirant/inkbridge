package org.inkbridge.android

import android.content.Context
import android.graphics.Color
import android.net.wifi.WifiManager
import android.os.Bundle
import android.view.Gravity
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.WindowManager
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
    /** Types on the computer; its button shows when the computer can type. */
    private lateinit var keyboard: KeyboardBridge
    private var keysChip: TextView? = null
    private var extraKeys: android.view.View? = null

    /** Opens the display menu; shown only when the computer has several. */
    private var screens: TextView? = null

    /** The bars over the picture; they fade while the pen is drawing. */
    private var controls: List<android.view.View> = emptyList()
    private val showControls = Runnable { controls.forEach { it.animate().alpha(1f).setDuration(200).start() } }

    /** Gets the bars out of the way of the pen, and back when it rests. */
    private fun dimControls() {
        controls.forEach { it.animate().cancel(); it.alpha = DIMMED }
        info.removeCallbacks(showControls)
        info.postDelayed(showControls, RESTORE_MS)
    }

    /** Shows or hides the tablet's keyboard together with the key row. */
    private fun setTyping(on: Boolean) {
        android.util.Log.d("Inkbridge", "keyboard ${if (on) "open" else "closed"}")
        if (on) keyboard.show() else keyboard.hide()
        extraKeys?.visibility = if (on) android.view.View.VISIBLE else android.view.View.GONE
        keysChip?.let { Ui.setChecked(it, on) }
        if (!on) hideSystemBars()
    }

    private fun dp(value: Float): Int = (value * resources.displayMetrics.density).toInt()

    /** Status text in the normal colour, or as a warning. */
    private fun status(text: String, warning: Boolean = false) {
        info.text = text
        info.setTextColor(if (warning) Ui.WARNING else Ui.TEXT)
        if (warning) { info.removeCallbacks(showControls); showControls.run() }
    }
    private var ticks = 0

    /** Whether the status shows every latency figure or only the total. */
    private var detailed = false
    private var lastFigures = ""
    private val ticker = object : Runnable {
        override fun run() {
            val s = SessionHolder.session
            if (s == null) {
                status("Disconnected", warning = true)
                return
            }
            if (!s.isConnected()) {
                status("Connection lost, reconnecting…", warning = true)
                reconnect()
                return
            }
            val host = s.host()
            screens?.visibility = if (s.displays().size > 1) android.view.View.VISIBLE else android.view.View.GONE
            keysChip?.visibility = if (s.keyboardAvailable()) android.view.View.VISIBLE else android.view.View.GONE
            if (s.keyboardAvailable()) keyboard.applyNames(s.keyNames())
            pad.onHostRegion(host.regionX, host.regionY, host.regionWidth, host.regionHeight)
            val link = if (host.usb) "USB" else "Wi-Fi"
            val full = if (s.hasVideo()) {
                val l = s.latency()
                "%s · %s · screen %.0f ms (p95 %.0f) · encode %.0f · net %.0f · %.0f fps · %.1f Mbit/s".format(
                    host.name, link, l.totalMs, l.totalP95Ms, l.encodeMs, l.networkMs, l.fps, l.bitrateMbps,
                )
            } else {
                "${host.name} · ${"%.1f".format(s.rttUs().toDouble() / 1000.0)} ms"
            }
            // On screen: the short form; tap it for every figure.
            status(
                if (detailed || !s.hasVideo()) full
                else "%s · %s · %.0f ms".format(host.name, link, s.latency().totalMs),
            )
            lastFigures = full
            if (++ticks % 8 == 0) android.util.Log.d("Inkbridge", lastFigures.substringAfter(" · "))
            // Fast enough that the area follows a rotation almost at once.
            info.postDelayed(this, 250)
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val session = SessionHolder.session ?: run { finish(); return }
        own = session
        window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        // The whole screen is the pad: don't leave room for system bars or
        // a vendor's window caption (they are hidden, or drawn over it).
        WindowCompat.setDecorFitsSystemWindows(window, false)

        pad = PenPadView(this, session)
        val screen = if (session.hasVideo()) SurfaceView(this) else null
        val ink = if (screen != null) InkLayer(this) else null
        pad.inkLayer = ink
        info = Ui.label(this, "", 12f, Ui.TEXT).apply { setPadding(dp(10f), dp(5f), dp(10f), dp(5f)) }
        val clear = Ui.chip(this, "Clear") { pad.clearInk() }
        // Zoomable view: "View" switches two-finger gestures to moving and
        // zooming the tablet's view of the desktop; "Fit" shows it all.
        val view = Ui.chip(this, "View") { chip ->
            pad.viewMode = !pad.viewMode
            Ui.setChecked(chip, pad.viewMode)
        }
        val fit = Ui.chip(this, "Fit") { pad.fitWholeDisplay() }
        // Which of the computer's displays to show and draw on.
        val choose = Ui.chip(this, "Screen") { chooseDisplay(session) }.apply {
            visibility = android.view.View.GONE
        }
        screens = choose
        keyboard = KeyboardBridge(this) { SessionHolder.session }
        val keys = keyboard.extraKeys(this) { setTyping(false) }.apply { visibility = android.view.View.GONE }
        extraKeys = keys
        val typing = Ui.chip(this, "Keys") { setTyping(!keyboard.shown) }
            .apply { visibility = android.view.View.GONE }
        keysChip = typing
        val tools = if (screen != null) {
            Ui.glassBar(this, clear, view, fit, choose, typing)
        } else {
            Ui.glassBar(this, clear, typing)
        }
        val status = Ui.glassBar(this, info)
        controls = listOf(tools, status)
        status.setOnClickListener { detailed = !detailed }
        pad.onPenDown = ::dimControls
        val margin = dp(8f)
        setContentView(FrameLayout(this).apply {
            setBackgroundColor(Color.BLACK)
            // The keyboard closed another way (its own ⌄ button, Back):
            // put the key row away too.
            var imeWasVisible = false
            androidx.core.view.ViewCompat.setOnApplyWindowInsetsListener(this) { _, insets ->
                val imeVisible = insets.isVisible(WindowInsetsCompat.Type.ime())
                if (imeWasVisible && !imeVisible && keyboard.shown) setTyping(false)
                imeWasVisible = imeVisible
                insets
            }
            screen?.let { addView(it, FrameLayout.LayoutParams(0, 0)) }
            ink?.let { addView(it.view) }
            addView(pad)
            addView(keyboard, FrameLayout.LayoutParams(1, 1))
            addView(keys, FrameLayout.LayoutParams(
                FrameLayout.LayoutParams.WRAP_CONTENT, FrameLayout.LayoutParams.WRAP_CONTENT,
                Gravity.TOP or Gravity.CENTER_HORIZONTAL,
            ).apply { setMargins(margin, margin * 6, margin, margin) })
            addView(tools, FrameLayout.LayoutParams(
                FrameLayout.LayoutParams.WRAP_CONTENT, FrameLayout.LayoutParams.WRAP_CONTENT, Gravity.TOP or Gravity.START,
            ).apply { setMargins(margin, margin, margin, margin) })
            addView(status, FrameLayout.LayoutParams(
                FrameLayout.LayoutParams.WRAP_CONTENT, FrameLayout.LayoutParams.WRAP_CONTENT, Gravity.TOP or Gravity.END,
            ).apply { setMargins(margin, margin, margin, margin) })
        })
        hideSystemBars()
        screen?.let { attachVideo(session, it) }

        onBackPressedDispatcher.addCallback(this, object : OnBackPressedCallback(true) {
            override fun handleOnBackPressed() {
                // Left on purpose: don't reconnect at the next start.
                Storage(this@PadActivity).openPad = null
                SessionHolder.session = null
                finish()
            }
        })
    }

    /** A hardware keyboard types on the computer too. */
    override fun onKeyDown(keyCode: Int, event: android.view.KeyEvent): Boolean {
        if (keyCode == android.view.KeyEvent.KEYCODE_BACK) return super.onKeyDown(keyCode, event)
        val s = SessionHolder.session
        if (s != null && s.keyboardAvailable() && keyboard.onKey(event)) return true
        return super.onKeyDown(keyCode, event)
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

        /** How see-through the bars are while the pen draws, and for how long after. */
        const val DIMMED = 0.15f
        const val RESTORE_MS = 1500L

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
