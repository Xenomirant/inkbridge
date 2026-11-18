package org.inkbridge.android

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

class ReconnectTest {
    @Test
    fun withTheCableLinkUpTheComputerIsAwaitedThere() {
        val tried = mutableListOf<String>()
        val session = Reconnect.attempt("192.168.42.129", { tried += "usb $it"; "usb" }, { tried += "wifi"; "wifi" })
        assertEquals("usb", session)
        assertEquals(listOf("usb 192.168.42.129"), tried)
    }

    @Test
    fun wifiIsTriedWhenTheComputerDoesNotDialOverTheCable() {
        assertEquals("wifi", Reconnect.attempt("192.168.42.129", { null }, { "wifi" }))
    }

    @Test
    fun withoutACableOnlyWifiIsTried() {
        val session = Reconnect.attempt<String>(null, { error("no cable link") }, { "wifi" })
        assertEquals("wifi", session)
    }

    @Test
    fun aRoundThatFailsBothWaysGivesNothing() {
        assertNull(Reconnect.attempt("192.168.42.129", { null }, { null }))
    }
}
