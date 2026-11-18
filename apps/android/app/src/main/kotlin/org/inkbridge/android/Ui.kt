package org.inkbridge.android

import android.content.Context
import android.content.res.ColorStateList
import android.graphics.Color
import android.graphics.Typeface
import android.graphics.drawable.GradientDrawable
import android.graphics.drawable.RippleDrawable
import android.view.Gravity
import android.view.View
import android.widget.LinearLayout
import android.widget.TextView

/** The app's look: one dark palette, rounded surfaces, one accent colour. */
object Ui {
    val BACKGROUND = Color.rgb(0x10, 0x11, 0x14)
    val SURFACE = Color.rgb(0x1C, 0x1E, 0x24)
    val OUTLINE = Color.rgb(0x2E, 0x31, 0x3A)
    val ACCENT = Color.rgb(0x6C, 0x9B, 0xFF)
    val TEXT = Color.rgb(0xEC, 0xEE, 0xF2)
    val MUTED = Color.rgb(0x9A, 0xA0, 0xAA)
    val WARNING = Color.rgb(0xFF, 0x8A, 0x7A)

    /** Over the computer's picture: dark glass that stays readable on any content. */
    val GLASS = Color.argb(0xB8, 0x12, 0x13, 0x17)

    fun Context.dp(value: Float): Int = (value * resources.displayMetrics.density).toInt()

    fun rounded(color: Int, radius: Float, outline: Int? = null, outlineWidth: Int = 1) =
        GradientDrawable().apply {
            setColor(color)
            cornerRadius = radius
            outline?.let { setStroke(outlineWidth, it) }
        }

    /** A rounded surface that shows a ripple when pressed. */
    private fun pressable(color: Int, radius: Float, outline: Int? = null) = RippleDrawable(
        ColorStateList.valueOf(Color.argb(0x40, 0xFF, 0xFF, 0xFF)),
        rounded(color, radius, outline),
        rounded(Color.WHITE, radius),
    )

    fun label(context: Context, value: CharSequence, size: Float, color: Int = TEXT, bold: Boolean = false) =
        TextView(context).apply {
            text = value
            textSize = size
            setTextColor(color)
            if (bold) typeface = Typeface.create("sans-serif-medium", Typeface.NORMAL)
        }

    /** The main action of a screen. */
    fun primaryButton(context: Context, value: CharSequence, onClick: () -> Unit) =
        label(context, value, 16f, BACKGROUND, bold = true).apply {
            gravity = Gravity.CENTER
            background = pressable(ACCENT, context.dp(14f).toFloat())
            setPadding(context.dp(24f), context.dp(14f), context.dp(24f), context.dp(14f))
            isClickable = true
            setOnClickListener { onClick() }
        }

    /** A tappable row with a title and a line of detail. */
    fun card(context: Context, title: CharSequence, detail: CharSequence, onClick: () -> Unit): LinearLayout =
        LinearLayout(context).apply {
            orientation = LinearLayout.VERTICAL
            background = pressable(SURFACE, context.dp(16f).toFloat(), OUTLINE)
            setPadding(context.dp(20f), context.dp(16f), context.dp(20f), context.dp(16f))
            isClickable = true
            addView(label(context, title, 18f, bold = true))
            addView(label(context, detail, 13f, MUTED).apply { setPadding(0, context.dp(4f), 0, 0) })
            setOnClickListener { onClick() }
        }

    /** A small button for the bar over the computer's picture. */
    fun chip(context: Context, value: CharSequence, onClick: (TextView) -> Unit) =
        label(context, value, 13f, TEXT, bold = true).apply {
            gravity = Gravity.CENTER
            background = pressable(Color.TRANSPARENT, context.dp(10f).toFloat())
            setPadding(context.dp(12f), context.dp(5f), context.dp(12f), context.dp(5f))
            isClickable = true
            setOnClickListener { onClick(this) }
        }

    /** Marks a chip as switched on or off. */
    fun setChecked(chip: TextView, checked: Boolean) {
        chip.setTextColor(if (checked) ACCENT else TEXT)
    }

    /** The glass strip that holds chips or status text over the picture. */
    fun glassBar(context: Context, vararg children: View) = LinearLayout(context).apply {
        orientation = LinearLayout.HORIZONTAL
        gravity = Gravity.CENTER_VERTICAL
        background = rounded(GLASS, context.dp(12f).toFloat())
        setPadding(context.dp(4f), context.dp(2f), context.dp(4f), context.dp(2f))
        children.forEach { addView(it) }
    }
}
