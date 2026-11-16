package org.inkbridge.android

import android.content.Context
import android.util.Base64
import org.inkbridge.core.DeviceIdentity
import org.inkbridge.core.generateIdentity

/**
 * App-private persistence: this tablet's identity and the computer it is
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

    /** The pairing code of the computer this tablet is paired with, if any. */
    var pairedCode: String?
        get() = prefs.getString(KEY_CODE, null)
        set(v) = prefs.edit().putString(KEY_CODE, v).apply()

    var pairedName: String?
        get() = prefs.getString(KEY_NAME, null)
        set(v) = prefs.edit().putString(KEY_NAME, v).apply()

    private fun encode(b: ByteArray) = Base64.encodeToString(b, Base64.NO_WRAP)
    private fun decode(s: String) = Base64.decode(s, Base64.NO_WRAP)

    private companion object {
        const val KEY_CERT = "identity.cert"
        const val KEY_KEY = "identity.key"
        const val KEY_CODE = "paired.code"
        const val KEY_NAME = "paired.name"
    }
}
