package org.inkbridge.android

import java.net.Inet4Address
import java.net.NetworkInterface

/** Android USB tethering: the tablet's own address on the cable link. */
object UsbLink {
    /** Interface names Android uses for USB tethering (RNDIS or NCM). */
    private val PREFIXES = listOf("rndis", "usb", "ncm")

    /**
     * Held while waiting for the computer to dial over the cable: the waits (home screen, pad,
     * reconnect) share one port, so they take turns instead of failing to bind it.
     */
    val listening = Any()

    /** IPv4 address of the active USB tethering interface, or null. */
    fun tetherAddress(): String? = runCatching {
        NetworkInterface.getNetworkInterfaces()?.toList().orEmpty()
            .filter { nif -> nif.isUp && PREFIXES.any { nif.name.startsWith(it) } }
            .flatMap { it.inetAddresses.toList() }
            .firstOrNull { it is Inet4Address && !it.isLoopbackAddress }
            ?.hostAddress
    }.getOrNull()
}
