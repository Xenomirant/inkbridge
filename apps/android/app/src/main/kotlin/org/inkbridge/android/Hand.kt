package org.inkbridge.android

import kotlin.math.PI

/** The hand the user draws with. */
enum class Hand(val label: String) {
    LEFT("Left"),
    RIGHT("Right"),
    ;

    /**
     * Where the top of a pen held in this hand points, as Android's AXIS_ORIENTATION
     * (0 = up the screen, clockwise positive): towards that hand's side and the user.
     */
    val penOrientation: Float
        get() = if (this == RIGHT) HELD_ORIENTATION else -HELD_ORIENTATION

    private companion object {
        const val HELD_ORIENTATION = (2 * PI / 3).toFloat() // 120°: right/left and a little down
    }
}

/**
 * Fills in the tilt direction for tablets that report only the tilt angle. The TB331FC's
 * Android hands apps AXIS_ORIENTATION = 0 on every event although its pen measures both tilt
 * axes; there the direction comes from the hand the user draws with. A tablet that reports
 * any direction at all is trusted from then on.
 */
class TiltDirection {
    @Volatile var hand: Hand = Hand.RIGHT

    @Volatile private var reported = false

    fun orientation(orientation: Float, tilt: Float): Float {
        if (orientation != 0f && tilt > 0f) reported = true
        return if (reported || tilt == 0f) orientation else hand.penOrientation
    }
}
