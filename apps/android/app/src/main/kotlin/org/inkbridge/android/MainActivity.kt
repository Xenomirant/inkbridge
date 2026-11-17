package org.inkbridge.android

import android.content.Intent
import android.graphics.Typeface
import android.os.Build
import android.os.Bundle
import android.view.Gravity
import android.widget.Button
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
    private var busy = false

    private val scan = registerForActivityResult(ActivityResultContracts.StartActivityForResult()) { r ->
        r.data?.getStringExtra(ScanActivity.EXTRA_CODE)?.let { pair(it) }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        storage = Storage(this)
        setContentView(buildUi())
        refresh()
        handleIntent(intent)
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

    private fun buildUi(): LinearLayout {
        val pad = (24 * resources.displayMetrics.density).toInt()
        return LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            gravity = Gravity.CENTER_HORIZONTAL
            setPadding(pad, pad * 2, pad, pad)
            addView(TextView(context).apply {
                text = "Inkbridge"; textSize = 32f; typeface = Typeface.DEFAULT_BOLD
            })
            addView(TextView(context).apply {
                text = "Use this tablet as a pen for your computer."; textSize = 16f; setPadding(0, pad / 3, 0, pad)
            })
            status = TextView(context).apply { textSize = 15f; setPadding(0, 0, 0, pad) }
            addView(status)
            hostList = LinearLayout(context).apply { orientation = LinearLayout.VERTICAL }
            addView(hostList)
            addView(Button(context).apply {
                text = "Pair with a computer (scan QR code)"
                setOnClickListener { scan.launch(Intent(this@MainActivity, ScanActivity::class.java)) }
            })
            addView(TextView(context).apply {
                text = "Core ${coreVersion()}"; textSize = 12f; alpha = 0.6f; setPadding(0, pad, 0, 0)
            })
        }
    }

    private fun refresh() {
        val hosts = storage.hosts()
        hostList.removeAllViews()
        for (host in hosts) {
            hostList.addView(Button(this).apply {
                text = "Connect to ${host.name}"
                isEnabled = !busy
                setOnClickListener { connect(host.code, pair = false, label = host.name) }
                setOnLongClickListener { confirmForget(host); true }
            })
        }
        if (!busy) status.text = if (hosts.isEmpty()) "Run Inkbridge on your computer and scan its QR code." else ""
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

    private fun deviceName(): String =
        "${Build.MANUFACTURER} ${Build.MODEL}".trim().replaceFirstChar { it.uppercase() }
}
