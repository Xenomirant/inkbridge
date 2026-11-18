package org.inkbridge.android

/** Text for a paired computer's card on the home screen. */
object HostCards {
    private const val MINUTE = 60_000L
    private const val HOUR = 60 * MINUTE
    private const val DAY = 24 * HOUR

    /** When the computer was last used, in words, with what a tap does. */
    fun detail(lastUsedMs: Long, nowMs: Long): String {
        val ago = (nowMs - lastUsedMs).coerceAtLeast(0)
        val used = when {
            ago < HOUR -> "Used just now"
            ago < DAY -> "Used ${ago / HOUR} h ago"
            ago < 2 * DAY -> "Used yesterday"
            else -> "Used ${ago / DAY} days ago"
        }
        return "$used · tap to connect, hold to forget"
    }
}
