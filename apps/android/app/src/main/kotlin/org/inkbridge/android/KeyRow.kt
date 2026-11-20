package org.inkbridge.android

import org.inkbridge.core.KeyNames

/** The modifier chips of the key row, by role, and what they are called. */
object KeyRow {
    /** Role (the chip's key in the row) to the label the computer uses. */
    fun labels(names: KeyNames): Map<String, String> = mapOf(
        "Win" to names.meta,
        "Ctrl" to names.ctrl,
        "Alt" to names.alt,
        "Shift" to names.shift,
    )

    /** Whether a label is a symbol (⌘, ⇧, ←) rather than a word (Ctrl). */
    fun isSymbol(label: String): Boolean = label.isNotEmpty() && label.none { it.isLetterOrDigit() }
}
