package org.inkbridge.android

import android.view.MotionEvent
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class StylusMapperTest {
    @Test
    fun everyPenButtonCountsAsTheBarrelButton() {
        assertTrue(StylusMapper.isBarrelPressed(MotionEvent.BUTTON_STYLUS_PRIMARY))
        assertTrue(StylusMapper.isBarrelPressed(MotionEvent.BUTTON_STYLUS_SECONDARY))
        assertTrue(StylusMapper.isBarrelPressed(MotionEvent.BUTTON_SECONDARY))
        assertFalse(StylusMapper.isBarrelPressed(0))
        assertFalse(StylusMapper.isBarrelPressed(MotionEvent.BUTTON_PRIMARY))
    }

    @Test
    fun tiltTowardsTheRightIsPositiveX() {
        // Orientation π/2: the pen top points right; 45° from vertical.
        val (x, y) = StylusMapper.cartesianTilt((Math.PI / 4).toFloat(), (Math.PI / 2).toFloat())
        assertEquals(45f, x, 0.01f)
        assertEquals(0f, y, 0.01f)
    }
}
