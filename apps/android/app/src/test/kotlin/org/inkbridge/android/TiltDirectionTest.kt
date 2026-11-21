package org.inkbridge.android

import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

class TiltDirectionTest {
    @Test
    fun withoutADirectionTheHandDecidesIt() {
        val d = TiltDirection()
        d.hand = Hand.RIGHT
        val (rx, ry) = StylusMapper.cartesianTilt(0.5f, d.orientation(0f, 0.5f))
        assertTrue("leans right", rx > 0f)
        assertTrue("leans towards the user", ry > 0f)
        d.hand = Hand.LEFT
        val (lx, ly) = StylusMapper.cartesianTilt(0.5f, d.orientation(0f, 0.5f))
        assertTrue("leans left", lx < 0f)
        assertEquals(ry, ly, 1e-6f)
    }

    @Test
    fun aReportedDirectionIsKept() {
        val d = TiltDirection()
        assertEquals(1f, d.orientation(1f, 0.5f), 0f)
        // Once the tablet has shown it reports directions, an upright lean is real.
        assertEquals(0f, d.orientation(0f, 0.5f), 0f)
    }

    @Test
    fun anUprightPenHasNoDirectionToFillIn() {
        assertEquals(0f, TiltDirection().orientation(0f, 0f), 0f)
    }
}
