//
// Copyright 2026 Signal Messenger, LLC
// SPDX-License-Identifier: AGPL-3.0-only
//

//! The OS's network interfaces, in the shape the injectable network takes
//! them. The watch builds without the ObjC SDK and so without its
//! RTCNetworkMonitor; this is the injectable network's BasicNetworkManager:
//! getifaddrs, filtered and classified by name the way WebRTC's own
//! `GetAdapterTypeFromName` (rtc_base/network.cc, the iOS arm) does it.
//!
//! One entry per (interface, address family), because the injectable network
//! keys its networks by name and holds one IP each (injectable_network.cc,
//! "TODO: Add more than one IP per network interface"): the IPv4 entry is
//! named after the interface, the IPv6 one `<name>/v6`. The name reaches
//! nothing but logs and candidate `network_name`s.

use std::{collections::BTreeMap, net::IpAddr};

use crate::webrtc::network::NetworkInterfaceType;

/// One address of one interface, as getifaddrs reports it.
#[derive(Debug, Clone)]
pub struct Address {
    pub name: String,
    pub up: bool,
    pub loopback: bool,
    pub ip: IpAddr,
}

/// One entry the injectable network is told about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Interface {
    pub typ: NetworkInterfaceType,
    pub ip: IpAddr,
    /// Higher is preferred; feeds the candidate priority
    /// (`Network::preference()` in port.cc).
    pub preference: u16,
}

impl std::fmt::Display for Interface {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?} {} p{}", self.typ, self.ip, self.preference)
    }
}

/// WebRTC's table: a stem followed by digits only. `en` is Wi-Fi as it is
/// on iOS (the watch has no Ethernet); `pdp_ip` is cellular; the tunnels
/// (`utun`, the iPhone companion tunnel among them) are VPN.
pub fn interface_type(name: &str) -> NetworkInterfaceType {
    match name.trim_end_matches(|c: char| c.is_ascii_digit()) {
        "lo" => NetworkInterfaceType::Loopback,
        "pdp_ip" => NetworkInterfaceType::Cellular,
        "utun" | "ipsec" | "tun" | "tap" => NetworkInterfaceType::Vpn,
        "en" => NetworkInterfaceType::Wifi,
        _ => NetworkInterfaceType::Unknown,
    }
}

fn type_rank(typ: NetworkInterfaceType) -> u16 {
    match typ {
        NetworkInterfaceType::Ethernet | NetworkInterfaceType::Wifi => 3,
        NetworkInterfaceType::Cellular => 2,
        NetworkInterfaceType::Vpn => 1,
        _ => 0,
    }
}

/// fc00::/7: the companion tunnel's fd74:: is one. Unreachable by any peer,
/// but a harmless host candidate, and the only address the tunnel has.
fn is_ula(ip: &IpAddr) -> bool {
    matches!(ip, IpAddr::V6(v6) if v6.segments()[0] & 0xfe00 == 0xfc00)
}

/// Address family precedence within a type, after RFC 6724 as WebRTC sorts
/// (`SortNetworks`): global IPv6, then IPv4, then ULA.
fn family_rank(ip: &IpAddr) -> u16 {
    match ip {
        IpAddr::V4(_) => 1,
        IpAddr::V6(_) if is_ula(ip) => 0,
        IpAddr::V6(_) => 2,
    }
}

/// An address WebRTC could gather on. Link-local is out (a host candidate
/// no peer can use, and a STUN source no server answers); ULA stays.
fn usable(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => !v4.is_unspecified() && !v4.is_loopback() && !v4.is_link_local(),
        IpAddr::V6(v6) => {
            !v6.is_unspecified()
                && !v6.is_loopback()
                && !v6.is_multicast()
                && v6.segments()[0] & 0xffc0 != 0xfe80
        }
    }
}

/// The entries for a set of addresses: up, not loopback, one address per
/// (interface, family) -- the first IPv4; for IPv6 the first global, else
/// the first ULA, which is what `Network::GetBestIP` would pick from the
/// full list.
pub fn select(addresses: impl IntoIterator<Item = Address>) -> BTreeMap<String, Interface> {
    let mut selected: BTreeMap<String, Interface> = BTreeMap::new();
    for address in addresses {
        if !address.up || address.loopback || !usable(&address.ip) {
            continue;
        }
        let typ = interface_type(&address.name);
        if matches!(typ, NetworkInterfaceType::Loopback) {
            continue;
        }
        let key = match address.ip {
            IpAddr::V4(_) => address.name.clone(),
            IpAddr::V6(_) => format!("{}/v6", address.name),
        };
        let candidate = Interface {
            typ,
            ip: address.ip,
            preference: type_rank(typ) * 4 + family_rank(&address.ip),
        };
        match selected.get(&key) {
            Some(existing) if existing.preference >= candidate.preference => {}
            _ => {
                selected.insert(key, candidate);
            }
        }
    }
    selected
}

/// Every address getifaddrs reports, with the flags the selection reads.
#[cfg(target_vendor = "apple")]
pub fn addresses() -> Vec<Address> {
    let mut addresses = Vec::new();
    unsafe {
        let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut ifap) != 0 {
            warn!("getifaddrs failed: {}", std::io::Error::last_os_error());
            return addresses;
        }
        let mut cursor = ifap;
        while !cursor.is_null() {
            let ifa = &*cursor;
            cursor = ifa.ifa_next;
            if ifa.ifa_addr.is_null() {
                continue;
            }
            let ip = match (*ifa.ifa_addr).sa_family as i32 {
                libc::AF_INET => {
                    let sin = &*(ifa.ifa_addr as *const libc::sockaddr_in);
                    IpAddr::from(std::net::Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr)))
                }
                libc::AF_INET6 => {
                    let sin6 = &*(ifa.ifa_addr as *const libc::sockaddr_in6);
                    IpAddr::from(std::net::Ipv6Addr::from(sin6.sin6_addr.s6_addr))
                }
                _ => continue,
            };
            let flags = ifa.ifa_flags as i32;
            addresses.push(Address {
                name: std::ffi::CStr::from_ptr(ifa.ifa_name)
                    .to_string_lossy()
                    .into_owned(),
                up: flags & libc::IFF_UP != 0,
                loopback: flags & libc::IFF_LOOPBACK != 0,
                ip,
            });
        }
        libc::freeifaddrs(ifap);
    }
    addresses
}

/// What the injectable network should know right now.
#[cfg(target_vendor = "apple")]
pub fn current() -> BTreeMap<String, Interface> {
    select(addresses())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(name: &str, ip: &str) -> Address {
        Address {
            name: name.to_string(),
            up: true,
            loopback: false,
            ip: ip.parse().unwrap(),
        }
    }

    #[test]
    fn names_classify_as_webrtc_does() {
        use NetworkInterfaceType::*;
        for (name, typ) in [
            ("en0", Wifi),
            ("en2", Wifi),
            ("pdp_ip0", Cellular),
            ("pdp_ip3", Cellular),
            ("utun4", Vpn),
            ("ipsec0", Vpn),
            ("lo0", Loopback),
            ("awdl0", Unknown),
            ("llw0", Unknown),
            ("anpi0", Unknown),
            ("ap1", Unknown),
            ("bridge0", Unknown),
            ("en", Wifi),
            ("", Unknown),
            ("pdp_ipx", Unknown),
        ] {
            assert_eq!(interface_type(name), typ, "{name:?}");
        }
    }

    /// A watch on Wi-Fi with cellular attached and the phone nearby.
    #[test]
    fn selects_one_address_per_interface_and_family() {
        let selected = select([
            addr("lo0", "127.0.0.1"),
            addr("lo0", "::1"),
            addr("en0", "fe80::1c2b:3d4e:5f60:7a8b"),
            addr("en0", "192.168.1.23"),
            addr("en0", "2600:1700:abcd:1::5"),
            addr("en0", "2600:1700:abcd:1:9876:5432:10fe:dcba"),
            addr("pdp_ip0", "fe80::2"),
            addr("pdp_ip0", "2607:fb90:1234::9"),
            addr("pdp_ip0", "192.0.0.2"),
            addr("utun4", "fd74:a1b2:c3d4::7"),
            addr("awdl0", "fe80::3"),
            addr("llw0", "fe80::4"),
        ]);
        let keys: Vec<&str> = selected.keys().map(String::as_str).collect();
        assert_eq!(keys, ["en0", "en0/v6", "pdp_ip0", "pdp_ip0/v6", "utun4/v6"]);
        assert_eq!(selected["en0"].typ, NetworkInterfaceType::Wifi);
        assert_eq!(
            selected["en0"].ip,
            "192.168.1.23".parse::<IpAddr>().unwrap()
        );
        // The first global IPv6, as listed.
        assert_eq!(
            selected["en0/v6"].ip,
            "2600:1700:abcd:1::5".parse::<IpAddr>().unwrap()
        );
        assert_eq!(selected["pdp_ip0"].typ, NetworkInterfaceType::Cellular);
        assert_eq!(selected["utun4/v6"].typ, NetworkInterfaceType::Vpn);
        assert!(is_ula(&selected["utun4/v6"].ip));
        // Wi-Fi over cellular over the tunnel; global v6 over v4 over ULA.
        assert!(selected["en0/v6"].preference > selected["en0"].preference);
        assert!(selected["en0"].preference > selected["pdp_ip0/v6"].preference);
        assert!(selected["pdp_ip0"].preference > selected["utun4/v6"].preference);
    }

    #[test]
    fn a_global_v6_beats_a_ula_listed_first() {
        let selected = select([addr("en0", "fd00::1"), addr("en0", "2001:db8::1")]);
        assert_eq!(
            selected["en0/v6"].ip,
            "2001:db8::1".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn down_and_link_local_are_out() {
        let mut down = addr("en0", "10.0.0.5");
        down.up = false;
        let selected = select([down, addr("en1", "169.254.7.7"), addr("pdp_ip0", "fe80::1")]);
        assert!(selected.is_empty(), "{selected:?}");
    }

    /// Cellular only: what a call away from Wi-Fi has to gather on.
    #[test]
    fn cellular_only_yields_cellular() {
        let selected = select([addr("pdp_ip0", "10.20.30.40")]);
        assert_eq!(selected.len(), 1);
        assert_eq!(selected["pdp_ip0"].typ, NetworkInterfaceType::Cellular);
    }

    /// Live, on the host: nothing this selects is loopback or link-local,
    /// and the keys are what the names say.
    #[cfg(target_vendor = "apple")]
    #[test]
    fn host_enumeration_is_clean() {
        for (key, interface) in current() {
            println!("{key}: {interface}");
            assert!(usable(&interface.ip));
            assert!(!key.starts_with("lo"));
            assert_eq!(key.ends_with("/v6"), interface.ip.is_ipv6());
        }
    }
}
