package org.inkbridge.android

import android.content.Context
import android.text.InputType
import android.view.KeyEvent
import android.view.View
import android.view.inputmethod.BaseInputConnection
import android.view.inputmethod.EditorInfo
import android.view.inputmethod.InputConnection
import android.view.inputmethod.InputMethodManager
import android.widget.LinearLayout
import android.widget.TextView
import org.inkbridge.core.KeyModifiers
import org.inkbridge.core.Session
import org.inkbridge.core.TabletKey

/**
 * Types on the computer with the tablet's own keyboard. An invisible view
 * takes the keyboard's input and sends it on: committed text as text (any
 * language, the tablet's layout), Backspace/Enter and other keys as keys.
 * Ctrl, Alt, Shift and the Windows key on the extra-keys row stay pressed
 * for the next key, as on a phone's terminal app.
 */
class KeyboardBridge(context: Context, private val session: () -> Session?) : View(context) {

    private var ctrl = false
    private var alt = false
    private var shift = false
    private var meta = false
    private val modifierChips = mutableMapOf<String, TextView>()

    init {
        isFocusable = true
        isFocusableInTouchMode = true
    }

    private val imm get() = context.getSystemService(Context.INPUT_METHOD_SERVICE) as InputMethodManager

    var shown = false
        private set

    fun show() {
        requestFocus()
        imm.showSoftInput(this, InputMethodManager.SHOW_IMPLICIT)
        shown = true
    }

    fun hide() {
        imm.hideSoftInputFromWindow(windowToken, 0)
        clearFocus()
        shown = false
    }

    override fun onCheckIsTextEditor() = true

    override fun onCreateInputConnection(outAttrs: EditorInfo): InputConnection {
        // Plain text without suggestions: keyboards send each character as
        // it is typed. Not a password field, so Chinese and other input
        // methods still work (they commit the chosen words).
        outAttrs.inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS
        outAttrs.imeOptions = EditorInfo.IME_FLAG_NO_EXTRACT_UI or EditorInfo.IME_FLAG_NO_FULLSCREEN or
            EditorInfo.IME_ACTION_NONE
        return Connection()
    }

    private inner class Connection : BaseInputConnection(this@KeyboardBridge, false) {
        private var composing: CharSequence = ""

        override fun commitText(text: CharSequence, newCursorPosition: Int): Boolean {
            composing = ""
            send(text.toString())
            return true
        }

        // Words being composed (pinyin, for example) show in the keyboard's
        // own bar; the computer gets them when they are committed.
        override fun setComposingText(text: CharSequence, newCursorPosition: Int): Boolean {
            composing = text
            return true
        }

        override fun finishComposingText(): Boolean {
            if (composing.isNotEmpty()) send(composing.toString())
            composing = ""
            return true
        }

        override fun deleteSurroundingText(beforeLength: Int, afterLength: Int): Boolean {
            repeat(beforeLength.coerceAtMost(MAX_REPEAT)) { press(TabletKey.Backspace) }
            repeat(afterLength.coerceAtMost(MAX_REPEAT)) { press(TabletKey.Delete) }
            return true
        }

        override fun sendKeyEvent(event: KeyEvent): Boolean {
            if (event.action == KeyEvent.ACTION_DOWN) onKey(event)
            return true
        }

        override fun performEditorAction(actionCode: Int): Boolean {
            press(TabletKey.Enter)
            return true
        }
    }

    /** A key from the soft keyboard or a hardware keyboard. */
    fun onKey(event: KeyEvent): Boolean {
        val key = when (event.keyCode) {
            KeyEvent.KEYCODE_DEL -> TabletKey.Backspace
            KeyEvent.KEYCODE_FORWARD_DEL -> TabletKey.Delete
            KeyEvent.KEYCODE_ENTER, KeyEvent.KEYCODE_NUMPAD_ENTER -> TabletKey.Enter
            KeyEvent.KEYCODE_TAB -> TabletKey.Tab
            KeyEvent.KEYCODE_ESCAPE -> TabletKey.Escape
            KeyEvent.KEYCODE_DPAD_UP -> TabletKey.Up
            KeyEvent.KEYCODE_DPAD_DOWN -> TabletKey.Down
            KeyEvent.KEYCODE_DPAD_LEFT -> TabletKey.Left
            KeyEvent.KEYCODE_DPAD_RIGHT -> TabletKey.Right
            KeyEvent.KEYCODE_MOVE_HOME -> TabletKey.Home
            KeyEvent.KEYCODE_MOVE_END -> TabletKey.End
            KeyEvent.KEYCODE_PAGE_UP -> TabletKey.PageUp
            KeyEvent.KEYCODE_PAGE_DOWN -> TabletKey.PageDown
            KeyEvent.KEYCODE_META_LEFT, KeyEvent.KEYCODE_META_RIGHT -> TabletKey.Meta
            in KeyEvent.KEYCODE_F1..KeyEvent.KEYCODE_F12 ->
                TabletKey.Function((event.keyCode - KeyEvent.KEYCODE_F1 + 1).toUByte())
            else -> null
        }
        val held = KeyModifiers(
            shift = event.isShiftPressed, ctrl = event.isCtrlPressed,
            alt = event.isAltPressed, meta = event.isMetaPressed,
        )
        if (key != null) {
            press(key, held)
            return true
        }
        val c = event.getUnicodeChar(event.metaState and KeyEvent.META_SHIFT_MASK)
        if (c == 0) return false
        // A hardware keyboard's shortcut, or plain typing.
        if (held.ctrl || held.alt || held.meta) {
            press(TabletKey.Char(String(Character.toChars(c))), held)
        } else {
            send(String(Character.toChars(c)))
        }
        return true
    }

    override fun onKeyDown(keyCode: Int, event: KeyEvent): Boolean =
        onKey(event) || super.onKeyDown(keyCode, event)

    /** Text, or one character as a shortcut while a modifier is held. */
    private fun send(text: String) {
        if (text.isEmpty()) return
        if ((ctrl || alt || meta) && text.codePointCount(0, text.length) == 1) {
            press(TabletKey.Char(text))
            return
        }
        if (text == "\n") {
            press(TabletKey.Enter)
            return
        }
        runCatching { session()?.typeText(text) }
        releaseModifiers()
    }

    /** Presses `key` with the modifiers held on the extra-keys row. */
    fun press(key: TabletKey, extra: KeyModifiers = KeyModifiers(false, false, false, false)) {
        val held = KeyModifiers(
            shift = shift || extra.shift, ctrl = ctrl || extra.ctrl,
            alt = alt || extra.alt, meta = meta || extra.meta,
        )
        runCatching { session()?.pressKey(key, held) }
        releaseModifiers()
    }

    private fun releaseModifiers() {
        if (!(ctrl || alt || shift || meta)) return
        ctrl = false; alt = false; shift = false; meta = false
        modifierChips.values.forEach { Ui.setChecked(it, false) }
    }

    /** The row of keys a tablet keyboard lacks, for the glass bar. */
    fun extraKeys(context: Context): LinearLayout {
        fun key(label: String, key: TabletKey) = Ui.chip(context, label) { press(key) }
        fun modifier(label: String, toggle: () -> Boolean) = Ui.chip(context, label) { chip ->
            Ui.setChecked(chip, toggle())
        }.also { modifierChips[label] = it }
        return Ui.glassBar(
            context,
            key("Esc", TabletKey.Escape),
            key("Tab", TabletKey.Tab),
            // Tap: the key alone (opens Start). Hold: keep it pressed for
            // the next key (Win+E, Win+S).
            Ui.chip(context, "Win") { press(TabletKey.Meta) }.apply {
                modifierChips["Win"] = this
                setOnLongClickListener { meta = !meta; Ui.setChecked(this, meta); true }
            },
            modifier("Ctrl") { ctrl = !ctrl; ctrl },
            modifier("Alt") { alt = !alt; alt },
            modifier("Shift") { shift = !shift; shift },
            key("←", TabletKey.Left),
            key("↑", TabletKey.Up),
            key("↓", TabletKey.Down),
            key("→", TabletKey.Right),
            key("Del", TabletKey.Delete),
            key("⏎", TabletKey.Enter),
        )
    }

    private companion object {
        /** Keyboards delete at most a word at a time; cap runaway requests. */
        const val MAX_REPEAT = 64
    }
}
