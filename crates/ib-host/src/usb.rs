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

#[cfg(target_os = "linux")]
mod platform {
    use std::net::Ipv4Addr;

    /// (driver name, IPv4 gateways) for every network interface. The kernel
    /// driver identifies tethering (`rndis_host`, `cdc_ncm`, `cdc_ether`);
    /// the gateway comes from the routing table.
    pub fn adapters() -> Vec<(String, Vec<Ipv4Addr>)> {
        let routes = std::fs::read_to_string("/proc/net/route").unwrap_or_default();
        let gateways = super::parse_proc_net_route(&routes);
        let Ok(entries) = std::fs::read_dir("/sys/class/net") else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter_map(|e| {
                let iface = e.file_name().to_string_lossy().into_owned();
                let driver = std::fs::read_link(e.path().join("device/driver")).ok()?;
                let driver = driver.file_name()?.to_string_lossy().into_owned();
                let gws = gateways
                    .iter()
                    .filter(|(i, _)| *i == iface)
                    .map(|(_, g)| *g)
                    .collect();
                Some((driver, gws))
            })
            .collect()
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
mod platform {
    use std::net::Ipv4Addr;

    /// macOS detection arrives with that host (M4).
    pub fn adapters() -> Vec<(String, Vec<Ipv4Addr>)> {
        Vec::new()
    }
}

/// (interface, gateway) pairs from Linux's `/proc/net/route` (hex, host
/// byte order), for routes that have a gateway.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_proc_net_route(text: &str) -> Vec<(String, Ipv4Addr)> {
    const RTF_GATEWAY: u32 = 0x2;
    text.lines()
        .skip(1)
        .filter_map(|line| {
            let cols: Vec<&str> = line.split_whitespace().collect();
            let (iface, gateway, flags) = (cols.first()?, cols.get(2)?, cols.get(3)?);
            let flags = u32::from_str_radix(flags, 16).ok()?;
            let gw = u32::from_str_radix(gateway, 16).ok()?;
            (flags & RTF_GATEWAY != 0 && gw != 0)
                .then(|| ((*iface).to_owned(), Ipv4Addr::from(gw.to_le_bytes())))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_linux_routing_table() {
        let table = "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\n\
                     usb0\t00000000\t8C98A8C0\t0003\t0\t0\t100\t00000000\n\
                     eth0\t0050CC0A\t00000000\t0001\t0\t0\t0\t00F8FFFF\n";
        assert_eq!(
            parse_proc_net_route(table),
            vec![("usb0".to_owned(), Ipv4Addr::new(192, 168, 152, 140))]
        );
    }

    #[test]
    fn recognises_android_tethering_adapters() {
        assert!(is_tether_adapter("rndis_host"));
        assert!(is_tether_adapter("cdc_ncm"));
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
