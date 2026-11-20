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

    @Test
    fun symbolsAreToldApartFromWords() {
        for (symbol in listOf("⌘", "⇧", "←", "⏎")) assertEquals(true, KeyRow.isSymbol(symbol))
        for (word in listOf("Ctrl", "Win", "Esc", "")) assertEquals(false, KeyRow.isSymbol(word))
    }
}
