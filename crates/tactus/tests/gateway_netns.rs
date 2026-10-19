//! Gateway lifecycle against real interface changes (chapter 01 §9), in a
//! private network namespace.
//!
//! Each test re-runs itself under `unshare -rn` (an unprivileged user + network
//! namespace), builds two veth pairs there, and drives a fake remote peer from
//! raw sockets on the far ends:
//!
//! ```text
//!   tactus gateway  va 10.9.0.1  <==veth==>  vb 10.9.0.2  fake peer X, capture
//!   (opened later)  vc 10.9.1.1  <==veth==>  vd 10.9.1.2  capture
//! ```
//!
//! `accept_local` lets a datagram sent out `vb` from 10.9.0.2 arrive on `va`
//! as genuine LAN traffic (looped-back multicast does not exercise the
//! receive path under test). Where namespaces are unavailable the tests skip,
//! unless `REQUIRE_NETNS=1`, which CI sets.
#![cfg(target_os = "linux")]

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use socket2::{Domain, Protocol, Socket, Type};
use tactus::wire::discovery::{self, Frame, Message, PeerState};
use tactus::wire::types::Id;

const CHILD: &str = "TACTUS_NETNS_CHILD";

/// True in the namespaced child, which runs the test body. In the parent,
/// re-runs this one test under `unshare -rn` and returns false.
fn in_netns(test: &str) -> bool {
    if std::env::var_os(CHILD).is_some() {
        return true;
    }
    let usable = Command::new("unshare")
        .args(["-rn", "ip", "link", "set", "lo", "up"])
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !usable {
        assert!(
            std::env::var_os("REQUIRE_NETNS").is_none(),
            "REQUIRE_NETNS set but `unshare -rn` with iproute2 is unavailable"
        );
        eprintln!("skipping {test}: no unprivileged network namespaces here");
        return false;
    }
    let exe = std::env::current_exe().unwrap();
    let status = Command::new("unshare")
        .arg("-rn")
        .arg(exe)
        .args([test, "--exact", "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .status()
        .expect("run test in namespace");
    assert!(status.success(), "{test} failed inside the namespace");
    false
}

fn ip(args: &str) {
    let ok = Command::new("ip")
        .args(args.split_whitespace())
        .status()
        .is_ok_and(|s| s.success());
    assert!(ok, "ip {args}");
}

/// Apply several `ip` commands in one process, so they land between the
/// same two rescans.
fn ip_batch(cmds: &str) {
    let mut child = Command::new("ip")
        .args(["-batch", "-"])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(cmds.as_bytes())
        .unwrap();
    assert!(child.wait().unwrap().success(), "ip -batch {cmds:?}");
}

fn setup_links() {
    ip("link set lo up");
    ip("link add va type veth peer name vb");
    ip("link add vc type veth peer name vd");
    for l in ["va", "vb", "vc", "vd"] {
        ip(&format!("link set {l} up"));
    }
    ip("addr add 10.9.0.1/24 dev va");
    ip("addr add 10.9.0.2/24 dev vb");
    ip("addr add 10.9.1.2/24 dev vd");
    for c in ["all", "default", "va", "vb", "vc", "vd"] {
        let base = format!("/proc/sys/net/ipv4/conf/{c}");
        std::fs::write(format!("{base}/accept_local"), "1").unwrap();
        std::fs::write(format!("{base}/rp_filter"), "0").unwrap();
    }
}

const X: Id = Id(*b"FAKEPEER");

/// Fake remote peer X on `vb`: Alives naming `session` from 16 source ports.
/// Distinct ports matter: a socket-selection bug that hashes the sender's
/// address and port misroutes some ports and not others.
fn spawn_fake_peer(session: Id, stop: Arc<AtomicBool>) -> JoinHandle<()> {
    let socks: Vec<UdpSocket> = (0..16)
        .map(|_| {
            let s = UdpSocket::bind("10.9.0.2:0").unwrap();
            s.set_multicast_loop_v4(false).unwrap();
            let s = Socket::from(s);
            s.set_multicast_if_v4(&Ipv4Addr::new(10, 9, 0, 2)).unwrap();
            s.into()
        })
        .collect();
    let alive = discovery::encode(&Frame::alive(
        X,
        PeerState {
            session: Some(session),
            ..PeerState::default()
        },
    ));
    let dst = SocketAddrV4::new(discovery::MULTICAST_V4, discovery::PORT);
    thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            for s in &socks {
                let _ = s.send_to(&alive, dst);
            }
            thread::sleep(Duration::from_millis(100));
        }
    })
}

type Captured = Arc<Mutex<Vec<(SocketAddr, Frame)>>>;

/// Record discovery frames arriving on the interface that holds `addr`.
fn spawn_capture(addr: Ipv4Addr, stop: Arc<AtomicBool>) -> (Captured, JoinHandle<()>) {
    let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).unwrap();
    s.set_reuse_address(true).unwrap();
    s.bind(&SocketAddr::from(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, discovery::PORT)).into())
        .unwrap();
    s.join_multicast_v4(&discovery::MULTICAST_V4, &addr)
        .unwrap();
    s.set_multicast_all_v4(false).unwrap();
    s.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
    let s: UdpSocket = s.into();
    let log: Captured = Arc::default();
    let out = log.clone();
    let h = thread::spawn(move || {
        let mut buf = [0u8; 600];
        while !stop.load(Ordering::Relaxed) {
            if let Ok((n, src)) = s.recv_from(&mut buf) {
                if let Ok(f) = discovery::decode(&buf[..n]) {
                    out.lock().unwrap().push((src, f));
                }
            }
        }
    });
    (log, h)
}

fn wait_for(limit: Duration, mut cond: impl FnMut() -> bool) -> Option<Duration> {
    let t0 = Instant::now();
    while t0.elapsed() < limit {
        if cond() {
            return Some(t0.elapsed());
        }
        thread::sleep(Duration::from_millis(20));
    }
    None
}

/// Joins a session with X over `va`, which is X's only path to us.
fn join_x(link: &tactus::Link, stop: &Arc<AtomicBool>) -> JoinHandle<()> {
    let x = spawn_fake_peer(Id(link.session_id()), stop.clone());
    assert!(
        wait_for(Duration::from_secs(3), || link.num_peers() == 1).is_some(),
        "X never became a session member"
    );
    // Let any misrouted copies of X's Alives settle into other gateways.
    thread::sleep(Duration::from_secs(1));
    x
}

/// Observations 1–3 (PR #6 interop): with the host's network address gone,
/// the next scan must close its gateway and, since that gateway held the only
/// session member, found a fresh session (chapter 01 §9.4 steps 2 and 4).
/// Before the fix, Linux delivered some of X's LAN Alives to the loopback
/// gateway's socket; that stray entry kept X a member, so the peer carried
/// its old NodeId and session onto the next network.
#[test]
fn losing_the_only_lan_path_founds_a_fresh_session() {
    if !in_netns("losing_the_only_lan_path_founds_a_fresh_session") {
        return;
    }
    setup_links();
    let stop = Arc::new(AtomicBool::new(false));
    let link = tactus::Link::new(120.0);
    link.enable();
    let x = join_x(&link, &stop);

    let node0 = link.node_id();
    ip("addr flush dev va");
    // X keeps sending: only a gateway close, never ttl expiry, can drop it.
    let after = wait_for(Duration::from_secs(7), || link.node_id() != node0);
    stop.store(true, Ordering::Relaxed);
    x.join().unwrap();

    let after = after.expect("no fresh session: an entry for X outlived its gateway");
    assert!(
        after <= Duration::from_millis(5_500),
        "reset {after:?} after the address went: later than one rescan"
    );
    assert_eq!(
        link.session_id(),
        link.node_id(),
        "a fresh session of our own"
    );
    assert_eq!(link.num_peers(), 0);
}

/// Chapter 01 §9.4 step 1 and §9.5: a scan closes `va` (no longer admitted:
/// multicast flag cleared, address kept, so its ByeBye can still be sent)
/// and, in the same scan, opens `vc`. The close sends a ByeBye under the old
/// NodeId; the lost session member forces a reset; `vc`'s first Alive already
/// carries the new NodeId and session.
#[test]
fn scan_close_says_byebye_and_resets_before_new_gateways_announce() {
    if !in_netns("scan_close_says_byebye_and_resets_before_new_gateways_announce") {
        return;
    }
    setup_links();
    let stop = Arc::new(AtomicBool::new(false));
    let link = tactus::Link::new(120.0);
    link.enable();
    let x = join_x(&link, &stop);
    let (on_vb, cap_b) = spawn_capture(Ipv4Addr::new(10, 9, 0, 2), stop.clone());
    let (on_vd, cap_d) = spawn_capture(Ipv4Addr::new(10, 9, 1, 2), stop.clone());

    let node0 = Id(link.node_id());
    ip_batch("link set va multicast off\naddr add 10.9.1.1/24 dev vc\n");
    let reset = wait_for(Duration::from_secs(7), || Id(link.node_id()) != node0);
    thread::sleep(Duration::from_millis(500));
    stop.store(true, Ordering::Relaxed);
    for h in [x, cap_b, cap_d] {
        h.join().unwrap();
    }
    assert!(
        reset.is_some(),
        "losing X with va did not reset the session"
    );
    let node1 = Id(link.node_id());

    let va: SocketAddr = "10.9.0.1:0".parse().unwrap();
    let byes: Vec<Id> = on_vb
        .lock()
        .unwrap()
        .iter()
        .filter(|(src, f)| src.ip() == va.ip() && f.message == Message::ByeBye)
        .map(|(_, f)| f.node)
        .collect();
    assert_eq!(
        byes,
        vec![node0],
        "one ByeBye from va, under the old NodeId"
    );

    let vc: SocketAddr = "10.9.1.1:0".parse().unwrap();
    let from_vc: Vec<Frame> = on_vd
        .lock()
        .unwrap()
        .iter()
        .filter(|(src, _)| src.ip() == vc.ip())
        .map(|(_, f)| f.clone())
        .collect();
    let first = from_vc.first().expect("vc never announced");
    assert_eq!(
        first.node, node1,
        "vc's first Alive is under the new NodeId"
    );
    match &first.message {
        Message::Alive(ps) => assert_eq!(ps.session, Some(node1), "and a fresh session"),
        m => panic!("vc's first frame is {m:?}, not an Alive"),
    }
    assert!(
        from_vc.iter().all(|f| f.node != node0),
        "no announcement on vc under the pre-reset identity"
    );
}
