package org.inkbridge.android

import android.content.Intent
import android.os.Build
import android.os.Bundle
import android.view.Gravity
import android.widget.LinearLayout
import android.widget.TextView
import android.app.AlertDialog
import androidx.activity.ComponentActivity
import androidx.activity.result.contract.ActivityResultContracts
import org.inkbridge.core.InkbridgeException
import org.inkbridge.core.Session
import org.inkbridge.core.coreVersion
import org.inkbridge.core.parsePairingCode
import kotlin.concurrent.thread

/** Home screen: pair by QR code, reconnect to a paired computer. */
class MainActivity : ComponentActivity() {

    private lateinit var storage: Storage
    private lateinit var status: TextView
    private lateinit var hostList: LinearLayout
    private lateinit var handLabel: TextView
    private var busy = false

    private val scan = registerForActivityResult(ActivityResultContracts.StartActivityForResult()) { r ->
        r.data?.getStringExtra(ScanActivity.EXTRA_CODE)?.let { pair(it) }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        storage = Storage(this)
        storage.hand?.let { StylusMapper.direction.hand = it }
        setContentView(buildUi())
        refresh()
        handleIntent(intent)
        if (storage.hand == null) askHand()
        // Android stopped the app while a pad was open (an update, low
        // memory): carry on where the user was.
        if (savedInstanceState == null && SessionHolder.session == null) {
            storage.openPad?.let { code ->
                val name = storage.hosts().firstOrNull { it.code == code }?.name ?: "computer"
                connect(code, pair = false, label = name)
            }
        }
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        handleIntent(intent)
    }

    override fun onResume() {
        super.onResume()
        refresh()
        startUsbWatcher()
    }

    override fun onPause() {
        super.onPause()
        usbWatching = false
    }

    @Volatile private var usbWatching = false

    /**
     * USB: while this screen is open, wait for a paired computer to dial in
     * over the tethering link (it dials the tablet: no firewall setup on the
     * computer). Connects without a tap as soon as the cable link is up.
     * The listener accepts one computer at a time, so it takes turns
     * between the paired ones.
     */
    private fun startUsbWatcher() {
        val hosts = storage.hosts()
        if (hosts.isEmpty()) return
        if (usbWatching) return
        usbWatching = true
        val identity = runCatching { storage.identity() }.getOrNull() ?: return
        val metrics = resources.displayMetrics
        thread(name = "inkbridge-usb") {
            var announced = false
            var turn = 0
            while (usbWatching) {
                val ip = UsbLink.tetherAddress()
                if (ip == null || busy) {
                    announced = false
                    Thread.sleep(1000)
                    continue
                }
                if (!announced) {
                    announced = true
                    runOnUiThread { if (!busy) status.text = "USB cable connected — waiting for the computer…" }
                }
                val code = hosts[turn++ % hosts.size].code
                val session = runCatching {
                    Session.acceptUsb(
                        code, identity, deviceName(), ip, 2000u,
                        VideoDecoder.screen(metrics.widthPixels, metrics.heightPixels),
                    )
                }.getOrNull() ?: continue
                usbWatching = false
                runOnUiThread { onConnected(session, code) }
            }
        }
    }

    /** `inkbridge:pair?...` links, e.g. from the system camera's QR scanner or adb. */
    private fun handleIntent(intent: Intent?) {
        val code = intent?.data?.toString() ?: return
        if (code.startsWith("inkbridge:")) pair(code)
    }

    private fun buildUi(): android.view.View {
        val pad = with(Ui) { dp(24f) }
        val column = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(pad, pad * 2, pad, pad)
            addView(Ui.label(context, "Inkbridge", 34f, bold = true))
            addView(Ui.label(context, "Draw on your computer with this tablet's pen.", 16f, Ui.MUTED).apply {
                setPadding(0, pad / 4, 0, pad)
            })
            status = Ui.label(context, "", 15f, Ui.ACCENT).apply { setPadding(0, 0, 0, pad / 2) }
            addView(status)
            hostList = LinearLayout(context).apply { orientation = LinearLayout.VERTICAL }
            addView(hostList)
            addView(
                Ui.primaryButton(context, "Pair with a computer") {
                    scan.launch(Intent(this@MainActivity, ScanActivity::class.java))
                },
                LinearLayout.LayoutParams(
                    LinearLayout.LayoutParams.MATCH_PARENT, LinearLayout.LayoutParams.WRAP_CONTENT,
                ).apply { topMargin = pad / 2 },
            )
            addView(Ui.label(context, "Scan the QR code that Inkbridge shows on the computer.", 13f, Ui.MUTED).apply {
                gravity = Gravity.CENTER
                setPadding(0, pad / 2, 0, 0)
            })
            handLabel = Ui.label(context, "", 13f, Ui.MUTED).apply {
                gravity = Gravity.CENTER
                setPadding(0, pad, 0, 0)
                setOnClickListener { askHand() }
            }
            addView(handLabel)
            showHand()
            addView(Ui.label(context, "Core ${coreVersion()}", 12f, Ui.OUTLINE).apply {
                gravity = Gravity.CENTER
                setPadding(0, pad, 0, 0)
            })
        }
        // A readable column in the middle of a wide tablet.
        return android.widget.ScrollView(this).apply {
            setBackgroundColor(Ui.BACKGROUND)
            isFillViewport = true
            addView(android.widget.FrameLayout(context).apply {
                addView(column, android.widget.FrameLayout.LayoutParams(
                    minOf(resources.displayMetrics.widthPixels, with(Ui) { dp(560f) }),
                    android.widget.FrameLayout.LayoutParams.WRAP_CONTENT,
                    Gravity.CENTER_HORIZONTAL,
                ))
            })
        }
    }

    private fun refresh() {
        val hosts = storage.hosts()
        hostList.removeAllViews()
        val gap = with(Ui) { dp(10f) }
        for (host in hosts) {
            val card = Ui.card(this, host.name, HostCards.detail(host.lastUsedMs, System.currentTimeMillis())) {
                if (!busy) connect(host.code, pair = false, label = host.name)
            }.apply {
                alpha = if (busy) 0.5f else 1f
                setOnLongClickListener { confirmForget(host); true }
            }
            hostList.addView(card, LinearLayout.LayoutParams(
                LinearLayout.LayoutParams.MATCH_PARENT, LinearLayout.LayoutParams.WRAP_CONTENT,
            ).apply { bottomMargin = gap })
        }
        if (!busy) status.text = if (hosts.isEmpty()) "Start Inkbridge on your computer to get its QR code." else ""
    }

    /**
     * Which hand the user draws with. Asked once at the first start; tapping the line on the
     * home screen changes it. Used for the pen's tilt direction where the tablet reports none.
     */
    private fun askHand() {
        val hands = Hand.entries
        AlertDialog.Builder(this)
            .setTitle("Which hand do you draw with?")
            .setItems(hands.map { it.label }.toTypedArray()) { _, i ->
                storage.hand = hands[i]
                StylusMapper.direction.hand = hands[i]
                showHand()
            }
            .show()
    }

    private fun showHand() {
        val hand = storage.hand
        handLabel.text = if (hand == null) "Drawing hand: not set (tap to choose)" else "Drawing hand: ${hand.label} (tap to change)"
    }

    /** Long-press on a computer: remove its pairing from this tablet. */
    private fun confirmForget(host: PairedHost) {
        AlertDialog.Builder(this)
            .setMessage("Forget ${host.name}? You can pair again by scanning its QR code.")
            .setPositiveButton("Forget") { _, _ -> storage.forget(host.fingerprint); refresh() }
            .setNegativeButton("Cancel", null)
            .show()
    }

    private fun pair(code: String) {
        val info = runCatching { parsePairingCode(code) }.getOrElse {
            status.text = "That QR code isn't an Inkbridge pairing code."; return
        }
        connect(code, pair = true, label = info.addresses.firstOrNull() ?: "computer")
    }

    private fun connect(code: String, pair: Boolean, label: String) {
        if (busy) return
        busy = true
        status.text = if (pair) "Pairing with $label…" else "Connecting to $label…"
        refresh()
        val metrics = resources.displayMetrics
        val identity = runCatching { storage.identity() }.getOrElse { fail(it); return }
        thread(name = "inkbridge-connect") {
            val result = runCatching {
                Session.connect(
                    code, identity, deviceName(), pair,
                    VideoDecoder.screen(metrics.widthPixels, metrics.heightPixels),
                )
            }
            runOnUiThread {
                busy = false
                result.onSuccess { session -> onConnected(session, code) }.onFailure { fail(it) }
                refresh()
            }
        }
    }

    private fun onConnected(session: Session, code: String) {
        val host = session.host()
        storage.remember(code, host.name)
        SessionHolder.close()
        SessionHolder.session = session
        SessionHolder.code = code
        storage.openPad = code
        status.text = "Connected to ${host.name}" + if (host.usb) " over USB" else ""
        startActivity(Intent(this, PadActivity::class.java))
    }

    private fun fail(t: Throwable) {
        busy = false
        status.text = when (t) {
            is InkbridgeException -> t.message ?: "Connection failed."
            else -> "Connection failed: ${t.message}"
        }
        refresh()
    }
}

/** This tablet's name as the computer shows it. */
internal fun deviceName(): String =
    "${Build.MANUFACTURER} ${Build.MODEL}".trim().replaceFirstChar { it.uppercase() }
