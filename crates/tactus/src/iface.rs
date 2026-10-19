//! Interface enumeration and gateway planning (spec chapter 00 §2,
//! chapter 01 §2).
//!
//! A gateway is one local IP address (chapter 00 §2). In
//! [`Gateways::Auto`](crate::Gateways::Auto) mode the peer derives its
//! gateway set from the host's interfaces and rescans every 5 s, opening
//! and closing gateways as addresses come and go (chapter 01 §2).
//!
//! The OS-facing part is the [`InterfaceSource`] trait; everything else here
//! — the admission filter and the add/remove diff — is pure and tested
//! against injected interface lists.

use std::collections::BTreeSet;
use std::io;
use std::net::Ipv4Addr;

/// One IPv4 address of one host interface, with the flags the admission
/// filter needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Interface {
    pub addr: Ipv4Addr,
    /// Administratively up *and* operationally running.
    pub up: bool,
    pub loopback: bool,
    pub multicast: bool,
    pub point_to_point: bool,
}

/// Where interface lists come from: the OS in production, a fixed list in
/// tests.
pub trait InterfaceSource: Send + Sync {
    fn interfaces(&self) -> io::Result<Vec<Interface>>;
}

/// The host's interfaces, via `getifaddrs` (unix) or
/// `GetAdaptersAddresses` (Windows).
pub struct SystemInterfaces;

impl InterfaceSource for SystemInterfaces {
    fn interfaces(&self) -> io::Result<Vec<Interface>> {
        sys::interfaces()
    }
}

/// The gateway addresses an interface list admits.
///
/// - Loopback is always a gateway, as exactly `127.0.0.1`: two peers on one
///   host discover each other through the loopback gateway (chapter 01 §2).
///   It is admitted whether or not the platform flags its loopback interface
///   multicast-capable (Linux `lo` carries no `IFF_MULTICAST` yet delivers
///   multicast), and even when enumeration fails.
/// - Any other address is admitted when its interface is up,
///   multicast-capable, and not point-to-point.
pub fn admit(ifaces: &[Interface]) -> BTreeSet<Ipv4Addr> {
    let mut out = BTreeSet::from([Ipv4Addr::LOCALHOST]);
    out.extend(
        ifaces
            .iter()
            .filter(|i| {
                i.up && i.multicast
                    && !i.point_to_point
                    && !i.loopback
                    && !i.addr.is_loopback()
                    && !i.addr.is_unspecified()
            })
            .map(|i| i.addr),
    );
    out
}

/// Gateways to open and close to get from `current` to `wanted`.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct GatewayDiff {
    pub added: Vec<Ipv4Addr>,
    pub removed: Vec<Ipv4Addr>,
}

impl GatewayDiff {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty()
    }
}

/// Set difference in both directions, each side sorted.
pub fn diff(current: &BTreeSet<Ipv4Addr>, wanted: &BTreeSet<Ipv4Addr>) -> GatewayDiff {
    GatewayDiff {
        added: wanted.difference(current).copied().collect(),
        removed: current.difference(wanted).copied().collect(),
    }
}

/// One rescan: enumerate, admit, diff against the open gateways. A failed
/// enumeration changes nothing — a transient OS error must not tear down
/// working gateways.
pub fn plan(source: &dyn InterfaceSource, current: &BTreeSet<Ipv4Addr>) -> GatewayDiff {
    match source.interfaces() {
        Ok(list) => diff(current, &admit(&list)),
        Err(_) => GatewayDiff::default(),
    }
}

/// The initial gateway set: what [`admit`] makes of the host's interfaces,
/// or loopback alone when enumeration fails.
pub fn initial(source: &dyn InterfaceSource) -> BTreeSet<Ipv4Addr> {
    admit(&source.interfaces().unwrap_or_default())
}

#[cfg(unix)]
mod sys {
    use super::Interface;
    use std::io;
    use std::net::Ipv4Addr;

    pub fn interfaces() -> io::Result<Vec<Interface>> {
        let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
        // SAFETY: getifaddrs writes a list head we own until freeifaddrs.
        if unsafe { libc::getifaddrs(&mut head) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let mut out = Vec::new();
        let mut cur = head;
        while !cur.is_null() {
            // SAFETY: `cur` is a node of the list returned above.
            let ifa = unsafe { &*cur };
            cur = ifa.ifa_next;
            if ifa.ifa_addr.is_null() {
                continue;
            }
            // SAFETY: non-null ifa_addr points at a sockaddr whose family
            // field is valid; AF_INET entries are sockaddr_in.
            let family = unsafe { (*ifa.ifa_addr).sa_family } as libc::c_int;
            if family != libc::AF_INET {
                continue;
            }
            let sin = unsafe { &*(ifa.ifa_addr as *const libc::sockaddr_in) };
            let addr = Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
            let flags = ifa.ifa_flags as libc::c_int;
            let has = |f: libc::c_int| flags & f != 0;
            out.push(Interface {
                addr,
                up: has(libc::IFF_UP) && has(libc::IFF_RUNNING),
                loopback: has(libc::IFF_LOOPBACK),
                multicast: has(libc::IFF_MULTICAST),
                point_to_point: has(libc::IFF_POINTOPOINT),
            });
        }
        // SAFETY: `head` came from getifaddrs and is freed exactly once.
        unsafe { libc::freeifaddrs(head) };
        Ok(out)
    }
}

#[cfg(windows)]
mod sys {
    use super::Interface;
    use std::io;
    use std::net::Ipv4Addr;

    use windows_sys::Win32::Foundation::{ERROR_BUFFER_OVERFLOW, NO_ERROR};
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        GetAdaptersAddresses, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER,
        GAA_FLAG_SKIP_MULTICAST, IF_TYPE_PPP, IF_TYPE_SOFTWARE_LOOPBACK, IF_TYPE_TUNNEL,
        IP_ADAPTER_ADDRESSES_LH, IP_ADAPTER_NO_MULTICAST,
    };
    use windows_sys::Win32::NetworkManagement::Ndis::IfOperStatusUp;
    use windows_sys::Win32::Networking::WinSock::{AF_INET, SOCKADDR_IN};

    pub fn interfaces() -> io::Result<Vec<Interface>> {
        let flags = GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER;
        // The list is variable-length; retry with the size the call asks
        // for (it can grow between calls). u64 storage keeps the buffer
        // aligned for IP_ADAPTER_ADDRESSES_LH.
        let mut size: u32 = 16 * 1024;
        let mut buf: Vec<u64>;
        let mut attempts = 0;
        loop {
            buf = vec![0u64; (size as usize).div_ceil(8)];
            // SAFETY: buf holds at least `size` writable, aligned bytes.
            let rc = unsafe {
                GetAdaptersAddresses(
                    AF_INET as u32,
                    flags,
                    std::ptr::null(),
                    buf.as_mut_ptr().cast(),
                    &mut size,
                )
            };
            match rc {
                NO_ERROR => break,
                ERROR_BUFFER_OVERFLOW if attempts < 3 => attempts += 1,
                e => return Err(io::Error::from_raw_os_error(e as i32)),
            }
        }

        let mut out = Vec::new();
        let mut cur = buf.as_ptr() as *const IP_ADAPTER_ADDRESSES_LH;
        while !cur.is_null() {
            // SAFETY: `cur` walks the list GetAdaptersAddresses wrote into
            // `buf`, which outlives this loop.
            let a = unsafe { &*cur };
            cur = a.Next;
            // SAFETY: `Flags` overlays the bitfield struct; both are u32.
            let adapter_flags = unsafe { a.Anonymous2.Flags };
            let up = a.OperStatus == IfOperStatusUp;
            let loopback = a.IfType == IF_TYPE_SOFTWARE_LOOPBACK;
            let multicast = adapter_flags & IP_ADAPTER_NO_MULTICAST == 0;
            let point_to_point = a.IfType == IF_TYPE_PPP || a.IfType == IF_TYPE_TUNNEL;
            let mut ua = a.FirstUnicastAddress;
            while !ua.is_null() {
                // SAFETY: unicast address nodes live in `buf`.
                let u = unsafe { &*ua };
                ua = u.Next;
                let sa = u.Address.lpSockaddr;
                // SAFETY: a non-null lpSockaddr is a SOCKADDR; AF_INET ones
                // are SOCKADDR_IN.
                if sa.is_null() || unsafe { (*sa).sa_family } != AF_INET {
                    continue;
                }
                let sin = unsafe { &*(sa as *const SOCKADDR_IN) };
                let addr = Ipv4Addr::from(u32::from_be(unsafe { sin.sin_addr.S_un.S_addr }));
                out.push(Interface {
                    addr,
                    up,
                    loopback,
                    multicast,
                    point_to_point,
                });
            }
        }
        Ok(out)
    }
}

#[cfg(not(any(unix, windows)))]
mod sys {
    use super::Interface;
    use std::io;

    pub fn interfaces() -> io::Result<Vec<Interface>> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "interface enumeration not supported on this platform",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `_name` only labels the case; the filter never looks at names.
    fn iface(_name: &str, addr: [u8; 4]) -> Interface {
        Interface {
            addr: Ipv4Addr::from(addr),
            up: true,
            loopback: false,
            multicast: true,
            point_to_point: false,
        }
    }

    fn set(addrs: &[[u8; 4]]) -> BTreeSet<Ipv4Addr> {
        addrs.iter().map(|a| Ipv4Addr::from(*a)).collect()
    }

    struct Fixed(io::Result<Vec<Interface>>);
    impl InterfaceSource for Fixed {
        fn interfaces(&self) -> io::Result<Vec<Interface>> {
            match &self.0 {
                Ok(v) => Ok(v.clone()),
                Err(e) => Err(io::Error::new(e.kind(), e.to_string())),
            }
        }
    }

    #[test]
    fn admits_up_multicast_broadcast_interfaces() {
        let list = vec![
            iface("en0", [192, 168, 1, 20]),
            iface("eth1", [10, 0, 0, 5]),
        ];
        assert_eq!(
            admit(&list),
            set(&[[127, 0, 0, 1], [10, 0, 0, 5], [192, 168, 1, 20]])
        );
    }

    #[test]
    fn rejects_down_non_multicast_and_point_to_point() {
        let down = Interface {
            up: false,
            ..iface("en1", [192, 168, 2, 2])
        };
        let no_mcast = Interface {
            multicast: false,
            ..iface("en2", [192, 168, 3, 3])
        };
        let ppp = Interface {
            point_to_point: true,
            ..iface("utun0", [10, 8, 0, 2])
        };
        let unspecified = iface("weird0", [0, 0, 0, 0]);
        assert_eq!(
            admit(&[down, no_mcast, ppp, unspecified]),
            set(&[[127, 0, 0, 1]])
        );
    }

    #[test]
    fn loopback_is_always_exactly_localhost() {
        // Linux `lo`: no IFF_MULTICAST, still the loopback gateway.
        let lo = Interface {
            loopback: true,
            multicast: false,
            ..iface("lo", [127, 0, 0, 1])
        };
        // Extra loopback addresses do not become further gateways.
        let lo_alias = Interface {
            loopback: true,
            ..iface("lo", [127, 0, 1, 1])
        };
        assert_eq!(admit(&[lo, lo_alias]), set(&[[127, 0, 0, 1]]));
        // Nothing enumerated at all: loopback regardless.
        assert_eq!(admit(&[]), set(&[[127, 0, 0, 1]]));
    }

    #[test]
    fn every_admitted_address_of_an_interface_is_a_gateway() {
        // A gateway is one local IP address (chapter 00 §2).
        let list = vec![
            iface("en0", [192, 168, 1, 20]),
            iface("en0", [192, 168, 1, 21]),
        ];
        assert_eq!(
            admit(&list),
            set(&[[127, 0, 0, 1], [192, 168, 1, 20], [192, 168, 1, 21]])
        );
    }

    #[test]
    fn diff_reports_both_directions() {
        let current = set(&[[127, 0, 0, 1], [192, 168, 1, 20]]);
        let wanted = set(&[[127, 0, 0, 1], [10, 0, 0, 5]]);
        assert_eq!(
            diff(&current, &wanted),
            GatewayDiff {
                added: vec![Ipv4Addr::new(10, 0, 0, 5)],
                removed: vec![Ipv4Addr::new(192, 168, 1, 20)],
            }
        );
        assert!(diff(&wanted, &wanted).is_empty());
    }

    #[test]
    fn plan_follows_interfaces_appearing_and_disappearing() {
        let lo_only = set(&[[127, 0, 0, 1]]);

        // A phone joins Wi-Fi.
        let joined = Fixed(Ok(vec![iface("wlan0", [192, 168, 1, 20])]));
        let d = plan(&joined, &lo_only);
        assert_eq!(d.added, vec![Ipv4Addr::new(192, 168, 1, 20)]);
        assert!(d.removed.is_empty());

        // The laptop moves to another network: address replaced.
        let open = set(&[[127, 0, 0, 1], [192, 168, 1, 20]]);
        let moved = Fixed(Ok(vec![iface("wlan0", [10, 1, 2, 3])]));
        assert_eq!(
            plan(&moved, &open),
            GatewayDiff {
                added: vec![Ipv4Addr::new(10, 1, 2, 3)],
                removed: vec![Ipv4Addr::new(192, 168, 1, 20)],
            }
        );

        // Wi-Fi drops: back to loopback alone; loopback never removed.
        let gone = Fixed(Ok(vec![Interface {
            up: false,
            ..iface("wlan0", [192, 168, 1, 20])
        }]));
        assert_eq!(
            plan(&gone, &open),
            GatewayDiff {
                added: vec![],
                removed: vec![Ipv4Addr::new(192, 168, 1, 20)],
            }
        );
    }

    #[test]
    fn failed_enumeration_changes_nothing() {
        let open = set(&[[127, 0, 0, 1], [192, 168, 1, 20]]);
        let broken = Fixed(Err(io::Error::other("enumeration failed")));
        assert!(plan(&broken, &open).is_empty());
        assert_eq!(initial(&broken), set(&[[127, 0, 0, 1]]));
    }

    /// Smoke test against the real OS: enumeration works here and loopback
    /// is among the admitted gateways.
    #[test]
    fn system_enumeration_admits_loopback() {
        let list = SystemInterfaces.interfaces().expect("enumerate interfaces");
        assert!(admit(&list).contains(&Ipv4Addr::LOCALHOST));
    }
}
