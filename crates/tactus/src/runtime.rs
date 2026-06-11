//! Threading: per-gateway receiver loops and the housekeeping timer.

use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use tactus_wire::types::{StartStopState, Timeline};

use crate::engine::{random_id, Engine, State};
use crate::net::Gateway;

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
                    if !st.enabled || st.epoch != epoch {
                        return;
                    }
                    dispatch(&eng, &mut st, gw.index, src, &buf[..n], role);
                }
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    let st = eng.lock();
                    if !st.enabled || st.epoch != epoch {
                        return;
                    }
                }
                Err(_) => return,
            }
        }
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

    let mut gateways = Vec::new();
    for (i, addr) in eng.config.gateways.iter().enumerate() {
        match Gateway::open(i, *addr) {
            Ok(gw) => gateways.push(Arc::new(gw)),
            Err(e) => eprintln!("tactus: cannot open gateway {addr}: {e}"),
        }
    }
    st.gateways = gateways;
    let epoch = st.epoch;
    for gw in st.gateways.clone() {
        for role in [
            Role::McastDiscovery,
            Role::UnicastDiscovery,
            Role::Measurement,
            Role::Audio,
        ] {
            spawn_receiver(eng.clone(), gw.clone(), role, epoch);
        }
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
    for gw in &st.gateways {
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
