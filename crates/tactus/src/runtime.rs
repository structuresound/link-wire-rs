//! Threading: per-gateway receiver loops, the housekeeping timer, and the
//! interface rescan loop.

use std::collections::{BTreeSet, HashSet};
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use tactus_wire::types::{StartStopState, Timeline};

use crate::engine::{random_id, Engine, State, RESCAN_PERIOD};
use crate::iface;
use crate::net::Gateway;
use crate::Gateways;

#[derive(Clone, Copy)]
enum Role {
    McastDiscovery,
    UnicastDiscovery,
    Measurement,
    Audio,
}

pub fn spawn_housekeeping(eng: Arc<Engine>) {
    std::thread::spawn(move || {
        let mut st = eng.lock();
        loop {
            if eng.shutdown.load(Ordering::Relaxed) {
                return;
            }
            let next = eng.housekeeping_pass(&mut st);
            let wait = (next - eng.now()).clamp(1_000, 250_000) as u64;
            let (guard, _) = eng
                .wake
                .wait_timeout(st, Duration::from_micros(wait))
                .unwrap_or_else(|e| e.into_inner());
            st = guard;
        }
    });
}

fn spawn_receiver(eng: Arc<Engine>, gw: Arc<Gateway>, role: Role, epoch: u64) {
    std::thread::spawn(move || {
        let socket = match role {
            Role::McastDiscovery => &gw.mcast_recv,
            Role::UnicastDiscovery => &gw.unicast,
            Role::Measurement => &gw.measurement,
            Role::Audio => &gw.audio,
        };
        let mut buf = [0u8; 2048];
        loop {
            match socket.recv_from(&mut buf) {
                Ok((n, src)) => {
                    let mut st = eng.lock();
                    if !gateway_live(&st, &gw, epoch) {
                        return;
                    }
                    dispatch(&eng, &mut st, gw.id, src, &buf[..n], role);
                }
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    let st = eng.lock();
                    if !gateway_live(&st, &gw, epoch) {
                        return;
                    }
                }
                Err(_) => return,
            }
        }
    });
}

/// A receiver keeps running while its epoch is current and its gateway is
/// still open; the last receiver to exit drops the sockets.
fn gateway_live(st: &State, gw: &Gateway, epoch: u64) -> bool {
    st.enabled && st.epoch == epoch && st.gateways.contains_key(&gw.id)
}

/// Open a gateway on `addr` and start its receivers.
fn open_gateway(
    eng: &Arc<Engine>,
    st: &mut State,
    addr: Ipv4Addr,
    epoch: u64,
) -> std::io::Result<Arc<Gateway>> {
    let gw = Arc::new(Gateway::open(st.next_gateway_id, addr)?);
    st.next_gateway_id += 1;
    st.gateways.insert(gw.id, gw.clone());
    for role in [
        Role::McastDiscovery,
        Role::UnicastDiscovery,
        Role::Measurement,
        Role::Audio,
    ] {
        spawn_receiver(eng.clone(), gw.clone(), role, epoch);
    }
    Ok(gw)
}

/// Wait on the engine until local time `until`; false once the epoch is
/// over (disabled, re-enabled, or shut down).
fn wait_in_epoch(eng: &Engine, epoch: u64, until: i64) -> bool {
    let mut st = eng.lock();
    loop {
        if eng.shutdown.load(Ordering::Relaxed) || !st.enabled || st.epoch != epoch {
            return false;
        }
        let left = until - eng.now();
        if left <= 0 {
            return true;
        }
        st = eng
            .wake
            .wait_timeout(st, Duration::from_micros(left as u64))
            .unwrap_or_else(|e| e.into_inner())
            .0;
    }
}

/// [`Gateways::Auto`]: rescan interfaces every 5 s and open/close gateways
/// as addresses come and go (chapter 01 §2). Enumeration runs without the
/// engine lock held. A failed scan changes nothing ([`iface::plan`]).
fn spawn_rescan(eng: Arc<Engine>, epoch: u64, mut failed: HashSet<Ipv4Addr>) {
    std::thread::spawn(move || loop {
        if !wait_in_epoch(&eng, epoch, eng.now() + RESCAN_PERIOD) {
            return;
        }
        // Only this thread changes the gateway set within an epoch, so the
        // snapshot stays valid while the lock is released for the scan.
        let current: BTreeSet<Ipv4Addr> = eng.lock().gateways.values().map(|g| g.addr).collect();
        let plan = iface::plan(eng.interfaces.as_ref(), &current);
        if plan.is_empty() {
            continue;
        }
        let mut st = eng.lock();
        if !st.enabled || st.epoch != epoch {
            return;
        }
        // Closures first, then openings (chapter 01 §9.3). A session reset
        // caused by the closures happens inside close_gateway, so the new
        // gateways' immediate first Alives already carry the post-reset
        // NodeId and session (§9.5).
        for addr in &plan.removed {
            let ids: Vec<usize> = st
                .gateways
                .values()
                .filter(|g| g.addr == *addr)
                .map(|g| g.id)
                .collect();
            for id in ids {
                eng.close_gateway(&mut st, id);
            }
        }
        // Addresses that failed to open are retried every scan, and
        // reported once per failure streak.
        failed.retain(|a| plan.added.contains(a));
        for addr in plan.added {
            match open_gateway(&eng, &mut st, addr, epoch) {
                Ok(gw) => {
                    failed.remove(&addr);
                    eng.send_alive_on(&st, &gw);
                }
                Err(e) => {
                    if failed.insert(addr) {
                        eprintln!("tactus: cannot open gateway {addr}: {e}");
                    }
                }
            }
        }
        eng.notify();
    });
}

fn dispatch(eng: &Engine, st: &mut State, gw: usize, src: SocketAddr, buf: &[u8], role: Role) {
    match role {
        Role::McastDiscovery | Role::UnicastDiscovery => {
            eng.handle_discovery_datagram(st, gw, src, buf)
        }
        Role::Measurement => eng.handle_measurement_datagram(st, gw, src, buf),
        Role::Audio => crate::audio::handle_datagram(eng, st, gw, src, buf),
    }
}

/// Join the network: fresh NodeId and session (enabling never imports prior
/// session state, chapter 02 §7), open gateways, start receivers, announce.
pub fn enable(eng: &Arc<Engine>) {
    let mut st = eng.lock();
    if st.enabled {
        return;
    }
    st.epoch += 1;
    st.enabled = true;
    st.node = random_id();
    st.session = st.node;
    let now = eng.now();
    st.ghost_offset = -now; // ghost 0 = founding moment (chapter 02 §2)
    st.timeline = Timeline {
        tempo: st.timeline.tempo, // tempo is local application state
        beat_origin: 0,
        time_origin: 0,
    };
    st.held_stst = StartStopState::default();
    st.session_member_seen = false;
    st.foreign_timelines.clear();
    st.measured.clear();
    st.measurement = None;
    st.pending.clear();
    st.peers.clear();
    st.broadcast_due = None;
    st.next_remeasure_at = None;

    let epoch = st.epoch;
    let auto = eng.config.gateways == Gateways::Auto;
    let addrs: Vec<Ipv4Addr> = match &eng.config.gateways {
        Gateways::Auto => iface::initial(eng.interfaces.as_ref())
            .into_iter()
            .collect(),
        // At most one gateway per address (chapter 01 §9.1): repeats in an
        // explicit list are dropped, keeping the first.
        Gateways::Explicit(addrs) => {
            let mut seen = HashSet::new();
            addrs.iter().copied().filter(|a| seen.insert(*a)).collect()
        }
    };
    let mut failed = HashSet::new();
    for addr in addrs {
        if let Err(e) = open_gateway(eng, &mut st, addr, epoch) {
            eprintln!("tactus: cannot open gateway {addr}: {e}");
            failed.insert(addr);
        }
    }
    if auto {
        spawn_rescan(eng.clone(), epoch, failed);
    }
    // First Alive goes out immediately on open (chapter 01 §4.1).
    eng.send_alive(&mut st, now);
    eng.notify();
}

/// Leave the network: ByeBye on every gateway (chapter 01 §4.3), close
/// sockets, forget peers.
pub fn disable(eng: &Arc<Engine>) {
    let mut st = eng.lock();
    if !st.enabled {
        return;
    }
    crate::audio::shutdown(eng, &mut st);
    let bye = tactus_wire::discovery::encode(&tactus_wire::discovery::Frame::bye_bye(st.node));
    for gw in st.gateways.values() {
        gw.send_multicast(&bye);
    }
    st.enabled = false;
    st.epoch += 1;
    st.gateways.clear();
    st.peers.clear();
    st.measurement = None;
    st.pending.clear();
    st.broadcast_due = None;
    st.session_member_seen = false;
    st.next_remeasure_at = None;
    eng.notify();
}
