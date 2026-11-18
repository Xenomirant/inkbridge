package org.inkbridge.android

import org.junit.Assert.assertEquals
import org.junit.Test

class HostCardsTest {
    private val hour = 60L * 60 * 1000
    private val now = 1000 * hour

    private fun used(agoMs: Long) = HostCards.detail(now - agoMs, now).substringBefore(" · ")

    @Test
    fun lastUseIsGivenInTheLargestFittingUnit() {
        assertEquals("Used just now", used(59 * 60 * 1000))
        assertEquals("Used 3 h ago", used(3 * hour))
        assertEquals("Used yesterday", used(30 * hour))
        assertEquals("Used 5 days ago", used(5 * 24 * hour))
    }

    @Test
    fun aClockThatWentBackwardsDoesNotShowNegativeTime() {
        assertEquals("Used just now", used(-5 * hour))
    }
}
