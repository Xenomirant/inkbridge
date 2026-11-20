package org.inkbridge.android

import org.inkbridge.core.KeyNames
import org.junit.Assert.assertEquals
import org.junit.Test

class KeyRowTest {
    @Test
    fun aMacNamesItsModifiersWithSymbols() {
        val mac = KeyNames(meta = "⌘", ctrl = "⌃", alt = "⌥", shift = "⇧")
        assertEquals(
            mapOf("Win" to "⌘", "Ctrl" to "⌃", "Alt" to "⌥", "Shift" to "⇧"),
            KeyRow.labels(mac),
        )
    }
}
