package org.inkbridge.android

/** How the pad gets a dropped connection back. */
object Reconnect {
    /**
     * One round of trying. With the USB cable link up ([tether] is the
     * tablet's address on it) the computer dials the tablet, so wait for
     * it there first; then dial the computer over Wi-Fi, which also covers
     * a cable that was unplugged or a computer that only listens.
     * Returns the new session, or null if neither way worked.
     */
    fun <T : Any> attempt(tether: String?, overUsb: (String) -> T?, overWifi: () -> T?): T? =
        tether?.let(overUsb) ?: overWifi()
}
