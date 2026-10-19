//! Gateway socket setup (spec chapter 01 §2, chapter 02 §3, chapter 03 §2).
//!
//! One *gateway* is one local interface address. Per gateway the peer runs:
//! a multicast receive socket bound to the discovery port, a unicast
//! discovery socket used for all sending, a measurement responder/initiator
//! socket, and (when LinkAudio is enabled) an audio socket. This
//! implementation currently supports IPv4 gateways.

use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::time::Duration;

use socket2::{Domain, Protocol, Socket, Type};
use tactus_wire::discovery;

/// Receive poll interval; receiver threads use it to notice shutdown.
const READ_TIMEOUT: Duration = Duration::from_millis(50);

pub struct Gateway {
    /// Stable identity for the gateway's lifetime; keys per-gateway peer,
    /// channel and path state. Never reused after the gateway closes.
    pub id: usize,
    /// The local interface address this gateway runs on.
    pub addr: Ipv4Addr,
    /// Bound to wildcard:20808, joined to the discovery group on this
    /// gateway's interface (chapter 01 §2 socket 1).
    pub mcast_recv: UdpSocket,
    /// Bound to an ephemeral port; used for *all* discovery sending and for
    /// receiving unicast Responses (chapter 01 §2 socket 2).
    pub unicast: UdpSocket,
    /// Measurement responder/initiator socket (chapter 02 §3).
    pub measurement: UdpSocket,
    /// LinkAudio endpoint socket (chapter 03 §2).
    pub audio: UdpSocket,
}

fn base_socket() -> io::Result<Socket> {
    let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    s.set_read_timeout(Some(READ_TIMEOUT))?;
    Ok(s)
}

fn ephemeral(addr: Ipv4Addr) -> io::Result<UdpSocket> {
    let s = base_socket()?;
    s.bind(&SocketAddr::from(SocketAddrV4::new(addr, 0)).into())?;
    Ok(s.into())
}

impl Gateway {
    pub fn open(id: usize, addr: Ipv4Addr) -> io::Result<Gateway> {
        let group = discovery::MULTICAST_V4;

        // Multicast receive socket: wildcard bind with port reuse so several
        // peers on one host share the discovery port (chapter 01 §2), group
        // joined on this gateway's interface, delivery restricted to groups
        // joined on this socket where the platform supports it.
        //
        // Both reuse options are required, and which one does the work depends
        // on the platform. Linux honours SO_REUSEADDR alone for a wildcard UDP
        // bind; macOS and the BSDs do NOT — there SO_REUSEADDR only relaxes the
        // TCP TIME_WAIT rule, and a second wildcard bind on the same UDP port
        // fails with EADDRINUSE unless every socket sharing it also sets
        // SO_REUSEPORT. So a process opening a second gateway (two LinkRuntimes
        // in one host, or a peer alongside another app) used to fail on macOS
        // and work on Linux. Set both; SO_REUSEPORT must be set before bind.
        //
        // Setting SO_REUSEPORT on Linux is safe here even though it enables
        // load balancing for unicast: nothing addresses this port by unicast.
        // Responses go to the sender's ephemeral `unicast` socket (engine.rs
        // replies to the datagram's source), and multicast delivery still fans
        // out to every socket joined to the group.
        let mcast = base_socket()?;
        mcast.set_reuse_address(true)?;
        // socket2 exposes set_reuse_port on unix minus the targets below; the
        // exclusion list is the union across socket2 0.6.x so a patch bump
        // cannot break the build.
        #[cfg(all(
            unix,
            not(any(
                target_os = "solaris",
                target_os = "illumos",
                target_os = "cygwin",
                target_os = "nuttx",
                target_os = "wasi"
            ))
        ))]
        mcast.set_reuse_port(true)?;
        mcast.bind(
            &SocketAddr::from(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, discovery::PORT)).into(),
        )?;
        mcast.join_multicast_v4(&group, &addr)?;
        #[cfg(target_os = "linux")]
        mcast.set_multicast_all_v4(false)?;

        // Unicast discovery socket: outbound multicast interface pinned to
        // this gateway; multicast loopback only on loopback gateways
        // (chapter 01 §2).
        let unicast = base_socket()?;
        unicast.bind(&SocketAddr::from(SocketAddrV4::new(addr, 0)).into())?;
        unicast.set_multicast_if_v4(&addr)?;
        unicast.set_multicast_loop_v4(addr.is_loopback())?;

        Ok(Gateway {
            id,
            addr,
            mcast_recv: mcast.into(),
            unicast: unicast.into(),
            measurement: ephemeral(addr)?,
            audio: ephemeral(addr)?,
        })
    }

    /// The measurement endpoint advertised as `mep4` (chapter 01 §6).
    pub fn measurement_endpoint(&self) -> SocketAddr {
        self.measurement.local_addr().expect("bound socket")
    }

    /// The audio endpoint advertised as `aep4` (chapter 03 §2).
    pub fn audio_endpoint(&self) -> SocketAddr {
        self.audio.local_addr().expect("bound socket")
    }

    /// Send a discovery message to the multicast group via this gateway.
    pub fn send_multicast(&self, bytes: &[u8]) {
        let dst = SocketAddrV4::new(discovery::MULTICAST_V4, discovery::PORT);
        let _ = self.unicast.send_to(bytes, dst);
    }

    /// Send a unicast discovery message (Response) from this gateway.
    pub fn send_unicast(&self, bytes: &[u8], dst: SocketAddr) {
        let _ = self.unicast.send_to(bytes, dst);
    }
}
