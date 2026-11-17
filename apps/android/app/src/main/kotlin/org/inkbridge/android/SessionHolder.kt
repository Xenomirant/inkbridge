package org.inkbridge.android

import org.inkbridge.core.Session
import kotlin.concurrent.thread

/** The one live session, shared between the connect screen and the pad. */
object SessionHolder {
    @Volatile
    var session: Session? = null

    /** The pairing code the session was made with, to connect again if it drops. */
    @Volatile
    var code: String? = null

    /** Ends the current session in the background (disconnect briefly blocks). */
    fun close() {
        val s = session ?: return
        session = null
        release(s)
    }

    /** Ends and frees a session nothing uses any more. */
    fun release(s: Session) {
        thread(name = "inkbridge-disconnect") {
            runCatching { s.disconnect() }
            s.close() // free the native session
        }
    }
}
