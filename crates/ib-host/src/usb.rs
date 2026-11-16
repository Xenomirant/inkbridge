//! Finding tablets on USB.
//!
//! With Android USB tethering on, the tablet is the computer's **gateway** on
//! a small private network over the cable. The host dials that address
//! (outgoing connections need no firewall rule) instead of waiting for the
//! tablet to find the host.

use std::net::Ipv4Addr;

/// A network link that looks like Android USB tethering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TetherLink {
    pub description: String,
    /// The tablet's address on the link (our gateway there).
    pub tablet: Ipv4Addr,
}

/// Whether an adapter description names Android USB tethering: RNDIS (most
/// devices) or CDC-NCM (Pixel 6+ and others).
pub fn is_tether_adapter(description: &str) -> bool {
    let d = description.to_ascii_lowercase();
    d.contains("remote ndis") || d.contains("rndis") || d.contains("ncm") || d.contains("android")
}

/// USB tethering links that are up, with a private IPv4 gateway.
pub fn tether_links() -> Vec<TetherLink> {
    platform::adapters()
        .into_iter()
        .filter(|(description, _)| is_tether_adapter(description))
        .flat_map(|(description, gateways)| {
            gateways
                .into_iter()
                .filter(Ipv4Addr::is_private)
                .map(move |tablet| TetherLink {
                    description: description.clone(),
                    tablet,
                })
        })
        .collect()
}

#[cfg(windows)]
mod platform {
    use std::net::Ipv4Addr;

    use windows::Win32::NetworkManagement::IpHelper::{
        GAA_FLAG_INCLUDE_GATEWAYS, GetAdaptersAddresses, IP_ADAPTER_ADDRESSES_LH,
    };
    use windows::Win32::NetworkManagement::Ndis::IfOperStatusUp;
    use windows::Win32::Networking::WinSock::AF_INET;

    /// (description, IPv4 gateways) of every adapter that is up.
    pub fn adapters() -> Vec<(String, Vec<Ipv4Addr>)> {
        let mut size = 16 * 1024u32;
        for _ in 0..3 {
            // u64 elements keep the buffer aligned for the adapter records.
            let mut buf = vec![0u64; (size as usize).div_ceil(8)];
            let first = buf.as_mut_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>();
            // SAFETY: the buffer is `size` bytes and suitably aligned.
            let r = unsafe {
                GetAdaptersAddresses(
                    u32::from(AF_INET.0),
                    GAA_FLAG_INCLUDE_GATEWAYS,
                    None,
                    Some(first),
                    &mut size,
                )
            };
            match r {
                0 => return collect(first),
                111 => {} // ERROR_BUFFER_OVERFLOW: retry; `size` now holds the need
                _ => return Vec::new(),
            }
        }
        Vec::new()
    }

    fn collect(mut p: *mut IP_ADAPTER_ADDRESSES_LH) -> Vec<(String, Vec<Ipv4Addr>)> {
        let mut out = Vec::new();
        // SAFETY: walking the linked list GetAdaptersAddresses filled in; it
        // stays valid while the caller's buffer lives.
        unsafe {
            while let Some(a) = p.as_ref() {
                if a.OperStatus == IfOperStatusUp {
                    let description = a.Description.to_string().unwrap_or_default();
                    let mut gateways = Vec::new();
                    let mut g = a.FirstGatewayAddress;
                    while let Some(gw) = g.as_ref() {
                        let sa = gw.Address.lpSockaddr;
                        if !sa.is_null() && (*sa).sa_family == AF_INET {
                            // sockaddr_in: family (2), port (2), address (4, network order).
                            // Read bytes: the record isn't guaranteed to be aligned.
                            let bytes = sa.cast::<u8>().add(4);
                            let addr: [u8; 4] = std::ptr::read_unaligned(bytes.cast::<[u8; 4]>());
                            gateways.push(Ipv4Addr::from(addr));
                        }
                        g = gw.Next;
                    }
                    out.push((description, gateways));
                }
                p = a.Next;
            }
        }
        out
    }
}

#[cfg(not(windows))]
mod platform {
    use std::net::Ipv4Addr;

    /// Linux/macOS detection arrives with those hosts (M3/M4).
    pub fn adapters() -> Vec<(String, Vec<Ipv4Addr>)> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_android_tethering_adapters() {
        assert!(is_tether_adapter(
            "Remote NDIS based Internet Sharing Device"
        ));
        assert!(is_tether_adapter("UsbNcm Host Device"));
        assert!(!is_tether_adapter(
            "Intel(R) Ethernet Connection (19) I219-LM"
        ));
        assert!(!is_tether_adapter(
            "Hyper-V Virtual Ethernet Container Adapter"
        ));
    }
}
