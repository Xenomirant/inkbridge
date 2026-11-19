package org.inkbridge.android

import android.content.Context
import android.util.Base64
import org.inkbridge.core.DeviceIdentity
import org.inkbridge.core.generateIdentity
import org.inkbridge.core.parsePairingCode

/**
 * App-private persistence: this tablet's identity and the computers it is
 * paired with. Stored in private app storage (not readable by other apps;
 * backups are disabled in the manifest).
 */
class Storage(context: Context) {
    private val prefs = context.getSharedPreferences("inkbridge", Context.MODE_PRIVATE)

    /** This tablet's identity, created on first use. */
    fun identity(): DeviceIdentity {
        val cert = prefs.getString(KEY_CERT, null)
        val key = prefs.getString(KEY_KEY, null)
        if (cert != null && key != null) return DeviceIdentity(decode(cert), decode(key))
        val id = generateIdentity()
        prefs.edit().putString(KEY_CERT, encode(id.certDer)).putString(KEY_KEY, encode(id.keyDer)).apply()
        return id
    }

    /**
     * Paired computers, most recently used first. Pairings unused for
     * [PairedHosts.EXPIRY_MS] are dropped here.
     */
    fun hosts(nowMs: Long = System.currentTimeMillis()): List<PairedHost> {
        val stored = prefs.getString(KEY_HOSTS, null)?.let(PairedHosts::decode) ?: migrate(nowMs)
        val current = PairedHosts.current(stored, nowMs)
        if (current != stored) save(current)
        return current
    }

    /** Records a successful pairing or connection to a computer. */
    fun remember(code: String, name: String, nowMs: Long = System.currentTimeMillis()) {
        val fingerprint = runCatching { parsePairingCode(code).hostFingerprint }.getOrNull() ?: return
        save(PairedHosts.remember(hosts(nowMs), PairedHost(code, name, fingerprint, nowMs)))
    }

    fun forget(fingerprint: String) {
        save(PairedHosts.forget(hosts(), fingerprint))
        val open = openPad
        if (open != null && runCatching { parsePairingCode(open).hostFingerprint }.getOrNull() == fingerprint) {
            openPad = null
        }
    }

    /**
     * The pairing code of the computer whose pad is open, while it is: set
     * when the pad opens, cleared when the user leaves it. If Android stops
     * the app meanwhile (an update, low memory), the next start reconnects.
     */
    var openPad: String?
        get() = prefs.getString(KEY_OPEN_PAD, null)
        set(value) = prefs.edit().putString(KEY_OPEN_PAD, value).apply()

    private fun save(hosts: List<PairedHost>) =
        prefs.edit().putString(KEY_HOSTS, PairedHosts.encode(hosts)).apply()

    /** Versions before the list kept a single pairing: carry it over. */
    private fun migrate(nowMs: Long): List<PairedHost> {
        val code = prefs.getString(KEY_CODE, null) ?: return emptyList()
        val fingerprint = runCatching { parsePairingCode(code).hostFingerprint }.getOrNull() ?: return emptyList()
        val name = prefs.getString(KEY_NAME, null) ?: "computer"
        return listOf(PairedHost(code, name, fingerprint, nowMs))
    }

    private fun encode(b: ByteArray) = Base64.encodeToString(b, Base64.NO_WRAP)
    private fun decode(s: String) = Base64.decode(s, Base64.NO_WRAP)

    private companion object {
        const val KEY_CERT = "identity.cert"
        const val KEY_KEY = "identity.key"
        const val KEY_CODE = "paired.code"
        const val KEY_NAME = "paired.name"
        const val KEY_OPEN_PAD = "open_pad"
        const val KEY_HOSTS = "paired.hosts"
    }
}
