package org.inkbridge.android

import org.inkbridge.core.Session
import kotlin.concurrent.thread

/** The one live session, shared between the connect screen and the pad. */
object SessionHolder {
    @Volatile
    var session: Session? = null

    /** Ends the session in the background (disconnect briefly blocks). */
    fun close() {
        val s = session ?: return
        session = null
        thread(name = "inkbridge-disconnect") {
            runCatching { s.disconnect() }
            s.close() // free the native session
        }
    }
}
