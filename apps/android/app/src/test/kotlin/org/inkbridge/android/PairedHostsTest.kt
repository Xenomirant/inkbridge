package org.inkbridge.android

import org.junit.Assert.assertEquals
import org.junit.Test

class PairedHostsTest {
    private val day = 24L * 60 * 60 * 1000
    private val pc = PairedHost("inkbridge:pair?pc", "Desk PC", "aa", lastUsedMs = 10 * day)
    private val mac = PairedHost("inkbridge:pair?mac", "MacBook", "bb", lastUsedMs = 20 * day)

    @Test
    fun pairingASecondComputerKeepsTheFirst() {
        val hosts = PairedHosts.remember(listOf(pc), mac)
        assertEquals(listOf(mac, pc), hosts)
    }

    @Test
    fun pairingTheSameComputerAgainReplacesItsEntry() {
        val again = pc.copy(code = "inkbridge:pair?new", lastUsedMs = 30 * day)
        assertEquals(listOf(again, mac), PairedHosts.remember(listOf(mac, pc), again))
    }

    @Test
    fun unusedPairingsExpireAndTheRestAreNewestFirst() {
        val now = 10 * day + PairedHosts.EXPIRY_MS
        assertEquals(listOf(mac), PairedHosts.current(listOf(pc, mac), now))
        assertEquals(listOf(mac, pc), PairedHosts.current(listOf(pc, mac), now - 1))
    }

    @Test
    fun forgetRemovesOnlyThatComputer() {
        assertEquals(listOf(mac), PairedHosts.forget(listOf(pc, mac), "aa"))
    }

    @Test
    fun listSurvivesStorageIncludingOddNames() {
        val odd = pc.copy(name = "Sergei's\tPC\nroom 2")
        val read = PairedHosts.decode(PairedHosts.encode(listOf(mac, odd)))
        assertEquals(listOf(mac, odd.copy(name = "Sergei's PC room 2")), read)
        assertEquals(emptyList<PairedHost>(), PairedHosts.decode(""))
        assertEquals(listOf(mac), PairedHosts.decode("garbage\n" + PairedHosts.encode(listOf(mac))))
    }
}
