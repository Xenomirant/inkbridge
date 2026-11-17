package org.inkbridge.android

/** A computer this tablet has paired with. */
data class PairedHost(
    /** The pairing code (addresses, fingerprint, secret) used to reconnect. */
    val code: String,
    val name: String,
    /** The computer's certificate fingerprint: its identity in the list. */
    val fingerprint: String,
    /** Last successful connection, milliseconds since the epoch. */
    val lastUsedMs: Long,
)

/**
 * The list of paired computers: most recently used first, one entry per
 * computer, and entries unused for [EXPIRY_MS] are dropped. Pure functions
 * (no Android types) so the rules are unit-tested.
 */
object PairedHosts {
    /** A pairing not used for this long is forgotten. */
    const val EXPIRY_MS = 90L * 24 * 60 * 60 * 1000

    /** Adds or refreshes [host] and puts it first. */
    fun remember(hosts: List<PairedHost>, host: PairedHost): List<PairedHost> =
        listOf(host) + hosts.filter { it.fingerprint != host.fingerprint }

    fun forget(hosts: List<PairedHost>, fingerprint: String): List<PairedHost> =
        hosts.filter { it.fingerprint != fingerprint }

    /** Drops expired entries; most recently used first. */
    fun current(hosts: List<PairedHost>, nowMs: Long): List<PairedHost> =
        hosts.filter { nowMs - it.lastUsedMs < EXPIRY_MS }.sortedByDescending { it.lastUsedMs }

    /** One line per computer: `lastUsed TAB fingerprint TAB name TAB code`. */
    fun encode(hosts: List<PairedHost>): String =
        hosts.joinToString("\n") { h ->
            listOf(h.lastUsedMs.toString(), clean(h.fingerprint), clean(h.name), clean(h.code)).joinToString("\t")
        }

    /** Reads [encode]'s output; malformed lines are skipped. */
    fun decode(text: String): List<PairedHost> =
        text.lineSequence().mapNotNull { line ->
            val f = line.split('\t')
            val lastUsed = f.getOrNull(0)?.toLongOrNull()
            if (f.size != 4 || lastUsed == null || f[1].isEmpty() || f[3].isEmpty()) {
                null
            } else {
                PairedHost(code = f[3], name = f[2], fingerprint = f[1], lastUsedMs = lastUsed)
            }
        }.toList()

    private fun clean(s: String) = s.replace('\t', ' ').replace('\n', ' ').replace('\r', ' ')
}
