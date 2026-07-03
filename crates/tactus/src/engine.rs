//! The peer state machine: discovery gossip processing (spec chapter 01),
//! clock measurement, session election, timeline and start/stop propagation
//! (chapter 02). All state lives behind one mutex; receiver threads and the
//! housekeeping thread call into the handlers here.

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

use tactus_wire::discovery::{self, PeerState};
use tactus_wire::sync;
use tactus_wire::types::{Id, NodeId, SessionId, StartStopState, Timeline};

use crate::math;
use crate::net::Gateway;
use crate::Config;

/// Join threshold ε (chapter 02 §7.2).
pub const JOIN_EPSILON: i64 = 500_000;
/// Re-measurement period for a joined session (chapter 02 §7.3).
pub const REMEASURE_PERIOD: i64 = 30_000_000;
/// Prune-timer padding (chapter 01 §7).
pub const PRUNE_PADDING: i64 = 1_000_000;
/// Nominal Alive period (chapter 01 §4.1).
pub const ALIVE_PERIOD: i64 = 250_000;
/// Minimum spacing between state-change broadcasts (chapter 01 §4.1).
pub const MIN_BROADCAST_SPACING: i64 = 50_000;
/// Measurement retry timer (chapter 02 §4.2).
pub const MEASUREMENT_RETRY: i64 = 50_000;
/// Timer-driven retries before a measurement fails (chapter 02 §4.2).
pub const MAX_MEASUREMENT_RETRIES: u32 = 5;

pub fn random_id() -> Id {
    let mut b = [0u8; 8];
    getrandom::fill(&mut b).expect("OS randomness");
    Id(b)
}

pub struct PeerEntry {
    pub state: PeerState,
    /// Expiry deadline on our clock, µs (chapter 01 §7).
    pub deadline: i64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    /// Measuring a foreign session to evaluate the join rule (ch. 02 §7.1).
    Join,
    /// Periodic re-measurement of the joined session (ch. 02 §7.3).
    Remeasure,
}

pub struct MeasureTask {
    pub session: SessionId,
    pub target: SocketAddr,
    pub gateway: usize,
    pub purpose: Purpose,
}

pub struct Measurement {
    pub task: MeasureTask,
    /// Offset samples, estimates of ghost − host µs (chapter 02 §5).
    pub samples: Vec<f64>,
    /// Timer-driven retries so far.
    pub timeouts: u32,
    pub retry_deadline: i64,
}

pub struct State {
    pub enabled: bool,
    /// Incremented on every enable/disable; receiver threads exit when it
    /// changes under them.
    pub epoch: u64,
    pub node: NodeId,
    pub session: SessionId,
    pub timeline: Timeline,
    /// Ghost transform intercept: ghost(t) = t + offset (slope fixed at 1,
    /// chapter 02 §2).
    pub ghost_offset: i64,
    /// The held network start/stop state (chapter 02 §8).
    pub held_stst: StartStopState,
    /// What the application sees as the transport state.
    pub app_playing: bool,
    pub sst_sync: bool,
    pub gateways: Vec<Arc<Gateway>>,
    pub peers: HashMap<(NodeId, usize), PeerEntry>,
    /// True once a member of the current session has been seen; the
    /// found-fresh-session reset (chapter 02 §7.3) fires only on the
    /// transition back to zero members.
    pub session_member_seen: bool,
    /// Latest gossiped timeline per foreign session, kept under the
    /// beat-origin priority rule (chapter 02 §7.2).
    pub foreign_timelines: HashMap<SessionId, Timeline>,
    /// Measured ghost offsets of known sessions (chapter 02 §7.2: sessions
    /// are retained with their measurement).
    pub measured: HashMap<SessionId, i64>,
    pub measurement: Option<Measurement>,
    pub pending: VecDeque<MeasureTask>,
    pub last_alive_at: i64,
    pub last_broadcast_at: i64,
    /// A state-change broadcast delayed by the 50 ms minimum spacing.
    pub broadcast_due: Option<i64>,
    pub next_remeasure_at: Option<i64>,
    /// Advertise an audio endpoint (LinkAudio enabled, chapter 03 §2).
    pub audio: Option<crate::audio::AudioState>,
}

pub struct Engine {
    pub state: Mutex<State>,
    pub wake: Condvar,
    pub shutdown: AtomicBool,
    pub config: Config,
    anchor: Instant,
}

impl Engine {
    pub fn new(bpm: f64, config: Config) -> Engine {
        let node = random_id();
        Engine {
            state: Mutex::new(State {
                enabled: false,
                epoch: 0,
                node,
                session: node,
                timeline: Timeline {
                    tempo: math::bpm_to_tempo(bpm),
                    beat_origin: 0,
                    time_origin: 0,
                },
                ghost_offset: 0,
                held_stst: StartStopState::default(),
                app_playing: false,
                sst_sync: false,
                gateways: Vec::new(),
                peers: HashMap::new(),
                session_member_seen: false,
                foreign_timelines: HashMap::new(),
                measured: HashMap::new(),
                measurement: None,
                pending: VecDeque::new(),
                last_alive_at: 0,
                last_broadcast_at: 0,
                broadcast_due: None,
                next_remeasure_at: None,
                audio: None,
            }),
            wake: Condvar::new(),
            shutdown: AtomicBool::new(false),
            config,
            anchor: Instant::now(),
        }
    }

    /// Local clock, microseconds since construction.
    pub fn now(&self) -> i64 {
        self.anchor.elapsed().as_micros() as i64
    }

    pub fn notify(&self) {
        self.wake.notify_all();
    }

    pub fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    // ------------------------------------------------------------ sending

    /// Our peer-state payload for one gateway (chapter 01 §6).
    pub fn build_state(&self, st: &State, gw: &Gateway) -> PeerState {
        PeerState {
            timeline: Some(st.timeline),
            session: Some(st.session),
            start_stop: Some(st.held_stst),
            measurement_endpoint: Some(gw.measurement_endpoint()),
            audio_endpoint: st.audio.as_ref().map(|_| gw.audio_endpoint()),
        }
    }

    /// Multicast an Alive on every gateway; resets the periodic timer.
    pub fn send_alive(&self, st: &mut State, now: i64) {
        for gw in &st.gateways {
            let state = self.build_state(st, gw);
            let frame = discovery::Frame::alive(st.node, state);
            gw.send_multicast(&discovery::encode(&frame));
        }
        st.last_alive_at = now;
        st.last_broadcast_at = now;
        st.broadcast_due = None;
    }

    /// Broadcast a state change immediately, or delay it to honor the 50 ms
    /// minimum spacing (chapter 01 §4.1).
    pub fn schedule_broadcast(&self, st: &mut State) {
        if !st.enabled {
            return;
        }
        let now = self.now();
        if now - st.last_broadcast_at >= MIN_BROADCAST_SPACING {
            self.send_alive(st, now);
        } else {
            st.broadcast_due = Some(st.last_broadcast_at + MIN_BROADCAST_SPACING);
            self.notify();
        }
    }

    // ---------------------------------------------------------- discovery

    pub fn handle_discovery_datagram(
        &self,
        st: &mut State,
        gw_idx: usize,
        src: SocketAddr,
        buf: &[u8],
    ) {
        if !st.enabled {
            return;
        }
        let Ok(header) = discovery::decode_header(buf) else {
            return;
        };
        // Admission rules (chapter 01 §3): self, foreign group → ignore.
        if header.node == st.node || header.group_id != 0 {
            return;
        }
        // Respond to an Alive before processing its payload, whether or not
        // the payload parses (chapter 01 §4.2).
        if header.is_alive() {
            if let Some(gw) = st.gateways.get(gw_idx).cloned() {
                let state = self.build_state(st, &gw);
                let resp = discovery::Frame::response(st.node, state);
                gw.send_unicast(&discovery::encode(&resp), src);
            }
        }
        let Ok(frame) = discovery::decode(buf) else {
            return; // payload parse failure: message discarded (ch. 00 §4.10)
        };
        match frame.message {
            discovery::Message::Alive(ps) | discovery::Message::Response(ps) => {
                self.process_peer_state(st, gw_idx, frame.node, frame.ttl, ps);
            }
            discovery::Message::ByeBye => {
                st.peers.remove(&(frame.node, gw_idx));
                crate::audio::peer_left(self, st, frame.node);
                self.check_session_loss(st);
                self.notify();
            }
        }
    }

    fn process_peer_state(
        &self,
        st: &mut State,
        gw_idx: usize,
        node: NodeId,
        ttl: u8,
        ps: PeerState,
    ) {
        let now = self.now();
        st.peers.insert(
            (node, gw_idx),
            PeerEntry {
                state: ps.clone(),
                deadline: now + ttl as i64 * 1_000_000,
            },
        );
        self.notify(); // peer deadlines changed; housekeeping re-plans

        let Some(sess) = ps.session else { return };
        if sess == st.session {
            st.session_member_seen = true;
            // Timeline priority: strictly greater beat origin wins
            // (chapter 02 §6 rule 2); receivers re-clamp tempo (rule 1).
            if let Some(tl) = ps.timeline {
                if tl.beat_origin > st.timeline.beat_origin {
                    st.timeline = Timeline {
                        tempo: math::clamp_tempo(tl.tempo),
                        ..tl
                    };
                    self.schedule_broadcast(st);
                }
            }
            // Start/stop: latest user action wins, every peer relays
            // (chapter 02 §8 rules 1–3).
            if let Some(stst) = ps.start_stop {
                if stst.timestamp > st.held_stst.timestamp {
                    st.held_stst = stst;
                    if st.sst_sync && stst != StartStopState::default() {
                        st.app_playing = stst.is_playing;
                    }
                    self.schedule_broadcast(st);
                }
            }
        } else {
            // Foreign session: cache its timeline under the same priority
            // rule (chapter 02 §7.2).
            if let Some(tl) = ps.timeline {
                st.foreign_timelines
                    .entry(sess)
                    .and_modify(|cur| {
                        if tl.beat_origin > cur.beat_origin {
                            *cur = tl;
                        }
                    })
                    .or_insert(tl);
            }
            if let Some(&offset) = st.measured.get(&sess) {
                // Already measured: the ghost-time difference is constant
                // (slope 1 both sides), so re-evaluating is cheap and
                // idempotent — and handles the case where *our* transform
                // changed since (session reset).
                self.evaluate_join(st, sess, offset);
            } else {
                self.queue_measurement(st, gw_idx, sess, &ps, Purpose::Join);
            }
        }
    }

    // -------------------------------------------------------- measurement

    fn queue_measurement(
        &self,
        st: &mut State,
        gw_idx: usize,
        sess: SessionId,
        ps: &PeerState,
        purpose: Purpose,
    ) {
        if st
            .measurement
            .as_ref()
            .is_some_and(|m| m.task.session == sess)
            || st.pending.iter().any(|t| t.session == sess)
        {
            return;
        }
        // Target the session's founder if visible, else the gossiping peer
        // (chapter 02 §7.1).
        let founder = st.peers.iter().find_map(|((n, g), e)| {
            (*n == sess)
                .then_some(e.state.measurement_endpoint.map(|m| (m, *g)))
                .flatten()
        });
        let target = founder.or(ps.measurement_endpoint.map(|m| (m, gw_idx)));
        let Some((target, gateway)) = target else {
            return;
        };
        st.pending.push_back(MeasureTask {
            session: sess,
            target,
            gateway,
            purpose,
        });
        self.start_next_measurement(st);
    }

    pub fn start_next_measurement(&self, st: &mut State) {
        if st.measurement.is_some() {
            return;
        }
        let Some(task) = st.pending.pop_front() else {
            return;
        };
        let Some(gw) = st.gateways.get(task.gateway) else {
            return;
        };
        let now = self.now();
        let _ = gw
            .measurement
            .send_to(&sync::encode_ping(now, None), task.target);
        st.measurement = Some(Measurement {
            task,
            samples: Vec::with_capacity(128),
            timeouts: 0,
            retry_deadline: now + MEASUREMENT_RETRY,
        });
        self.notify();
    }

    pub fn handle_measurement_datagram(
        &self,
        st: &mut State,
        gw_idx: usize,
        src: SocketAddr,
        buf: &[u8],
    ) {
        if !st.enabled {
            return;
        }
        let Ok(msg) = sync::decode(buf) else { return };
        match msg {
            sync::Message::Ping(ping) => {
                // Stateless responder: answer any ping with payload ≤ 32
                // bytes, echoing it verbatim (chapter 02 §4.3).
                if ping.payload.len() > sync::MAX_PING_PAYLOAD {
                    return;
                }
                let ghost = self.now() + st.ghost_offset;
                let pong = sync::encode_pong(st.session, ghost, &ping.payload);
                if let Some(gw) = st.gateways.get(gw_idx) {
                    let _ = gw.measurement.send_to(&pong, src);
                }
            }
            sync::Message::Pong(pong) => self.handle_pong(st, gw_idx, src, pong),
        }
    }

    fn handle_pong(&self, st: &mut State, gw_idx: usize, src: SocketAddr, pong: sync::Pong) {
        let now = self.now();
        {
            let Some(m) = st.measurement.as_ref() else {
                return;
            };
            // Pong admission: we take the §4.2 [N] latitude to correlate
            // pongs by the measured peer's endpoint — pings are only ever
            // sent there, so this filters nothing but foreign traffic and
            // avoids the reference's concurrent-measurement mutual abort.
            if m.task.gateway != gw_idx || m.task.target != src {
                return;
            }
            // Session check (chapter 02 §4.2): a pong naming a different
            // session fails the measurement immediately.
            if pong.session != m.task.session {
                let failed = st.measurement.take().unwrap();
                self.measurement_failed(st, failed);
                return;
            }
        }
        let m = st.measurement.as_mut().unwrap();
        // Offset samples (chapter 02 §5): two midpoint estimators per pong.
        let gt = pong.ghost_time;
        let (pht, pgt) = sync::parse_echo(&pong.echo).unwrap_or((None, None));
        if gt != 0 {
            if let Some(pht) = pht.filter(|&v| v != 0) {
                m.samples.push(gt as f64 - (now as f64 + pht as f64) / 2.0);
                if let Some(pgt) = pgt.filter(|&v| v != 0) {
                    m.samples.push((gt as f64 + pgt as f64) / 2.0 - pht as f64);
                }
            }
        }
        if m.samples.len() > sync::SAMPLES_REQUIRED {
            let mut done = st.measurement.take().unwrap();
            let offset = median_round(&mut done.samples);
            self.measurement_succeeded(st, done, offset);
        } else {
            // Chain immediately: next ping echoes this pong's ghost time
            // (chapter 02 §4.2).
            let (gateway, target) = (m.task.gateway, m.task.target);
            m.retry_deadline = now + MEASUREMENT_RETRY;
            if let Some(gw) = st.gateways.get(gateway) {
                let _ = gw
                    .measurement
                    .send_to(&sync::encode_ping(now, Some(gt)), target);
            }
            self.notify();
        }
    }

    fn measurement_succeeded(&self, st: &mut State, done: Measurement, offset: i64) {
        st.measured.insert(done.task.session, offset);
        match done.task.purpose {
            Purpose::Join => self.evaluate_join(st, done.task.session, offset),
            Purpose::Remeasure => {
                if st.session == done.task.session {
                    st.ghost_offset = offset;
                }
                st.next_remeasure_at = Some(self.now() + REMEASURE_PERIOD);
            }
        }
        self.start_next_measurement(st);
    }

    pub fn measurement_failed(&self, st: &mut State, failed: Measurement) {
        match failed.task.purpose {
            // Forgotten; re-measured if seen in gossip again (ch. 02 §7.1).
            Purpose::Join => {}
            // Schedule another attempt rather than abandoning the session
            // (chapter 02 §7.3).
            Purpose::Remeasure => st.next_remeasure_at = Some(self.now() + REMEASURE_PERIOD),
        }
        self.start_next_measurement(st);
    }

    // ---------------------------------------------------- session election

    /// The join rule (chapter 02 §7.2): the session with greater ghost time
    /// wins, byte-wise lesser id as the tie-break within ±ε.
    fn evaluate_join(&self, st: &mut State, sess: SessionId, offset: i64) {
        if sess == st.session {
            return;
        }
        // g_new − g_cur at any instant is the offset difference (slope 1).
        let diff = offset - st.ghost_offset;
        let join = diff > JOIN_EPSILON || (diff.abs() < JOIN_EPSILON && sess < st.session);
        if !join {
            return;
        }
        // Retain the abandoned session with its (exact or measured)
        // transform (chapter 02 §7.2).
        let old = st.session;
        let old_offset = st.ghost_offset;
        st.measured.insert(old, old_offset);
        st.session = sess;
        st.ghost_offset = offset;
        if let Some(tl) = st.foreign_timelines.get(&sess) {
            st.timeline = Timeline {
                tempo: math::clamp_tempo(tl.tempo),
                ..*tl
            };
        }
        // Joining resets the local start/stop state (chapter 02 §8 rule 4).
        st.held_stst = StartStopState::default();
        st.session_member_seen = true;
        st.next_remeasure_at = Some(self.now() + REMEASURE_PERIOD);
        self.schedule_broadcast(st);
    }

    /// Found a fresh session (chapter 02 §7.3): new NodeId, new transform,
    /// timeline continuing the local beat/tempo seamlessly.
    fn reset_session(&self, st: &mut State) {
        let now = self.now();
        let beats_now = math::beats_at_ghost(&st.timeline, now + st.ghost_offset);
        st.measured.insert(st.session, st.ghost_offset);
        st.node = random_id();
        st.session = st.node;
        st.ghost_offset = -now; // ghost time 0 = founding moment (ch. 02 §2)
        st.timeline = Timeline {
            tempo: st.timeline.tempo,
            beat_origin: beats_now,
            time_origin: 0,
        };
        st.held_stst = StartStopState {
            is_playing: st.held_stst.is_playing,
            beats: beats_now,
            timestamp: 0,
        };
        st.session_member_seen = false;
        st.next_remeasure_at = None;
        self.schedule_broadcast(st);
    }

    pub fn check_session_loss(&self, st: &mut State) {
        if !st.session_member_seen {
            return;
        }
        let session = st.session;
        let any_member = st.peers.values().any(|e| e.state.session == Some(session));
        if !any_member {
            self.reset_session(st);
        }
    }

    // ------------------------------------------------------- housekeeping

    /// One housekeeping pass; returns the next deadline (µs on our clock).
    pub fn housekeeping_pass(&self, st: &mut State) -> i64 {
        let now = self.now();
        let mut next = now + ALIVE_PERIOD;
        if !st.enabled {
            return next;
        }

        // Periodic Alive (chapter 01 §4.1).
        if now >= st.last_alive_at + ALIVE_PERIOD {
            self.send_alive(st, now);
        }
        next = next.min(st.last_alive_at + ALIVE_PERIOD);

        // Delayed state-change broadcast.
        if let Some(due) = st.broadcast_due {
            if now >= due {
                self.send_alive(st, now);
            } else {
                next = next.min(due);
            }
        }

        // Peer pruning: timer at earliest deadline + 1 s (chapter 01 §7).
        if let Some(earliest) = st.peers.values().map(|e| e.deadline).min() {
            if now >= earliest + PRUNE_PADDING {
                let expired: Vec<NodeId> = st
                    .peers
                    .iter()
                    .filter(|(_, e)| e.deadline <= now)
                    .map(|((n, _), _)| *n)
                    .collect();
                st.peers.retain(|_, e| e.deadline > now);
                for node in expired {
                    if !st.peers.keys().any(|(n, _)| *n == node) {
                        crate::audio::peer_left(self, st, node);
                    }
                }
                self.check_session_loss(st);
            }
            if let Some(e) = st.peers.values().map(|e| e.deadline).min() {
                next = next.min(e + PRUNE_PADDING);
            }
        }

        // Measurement retry timer (chapter 02 §4.2).
        if let Some((deadline, timeouts)) = st
            .measurement
            .as_ref()
            .map(|m| (m.retry_deadline, m.timeouts))
        {
            if now >= deadline {
                if timeouts >= MAX_MEASUREMENT_RETRIES {
                    let failed = st.measurement.take().unwrap();
                    self.measurement_failed(st, failed);
                } else {
                    let m = st.measurement.as_mut().unwrap();
                    m.timeouts += 1;
                    m.retry_deadline = now + MEASUREMENT_RETRY;
                    let (gateway, target) = (m.task.gateway, m.task.target);
                    if let Some(gw) = st.gateways.get(gateway) {
                        // Fresh ping without _pgt (chapter 02 §4.2).
                        let _ = gw
                            .measurement
                            .send_to(&sync::encode_ping(now, None), target);
                    }
                    next = next.min(now + MEASUREMENT_RETRY);
                }
            } else {
                next = next.min(deadline);
            }
        }

        // Periodic re-measurement of a joined session (chapter 02 §7.3).
        if let Some(t) = st.next_remeasure_at {
            if now >= t {
                self.queue_remeasure(st);
                st.next_remeasure_at = Some(now + REMEASURE_PERIOD);
                next = next.min(now + REMEASURE_PERIOD);
            } else {
                next = next.min(t);
            }
        }

        self.start_next_measurement(st);

        next.min(crate::audio::housekeeping(self, st, now))
    }

    fn queue_remeasure(&self, st: &mut State) {
        if st.session == st.node {
            return; // founders do not re-measure their own session
        }
        let sess = st.session;
        if st
            .measurement
            .as_ref()
            .is_some_and(|m| m.task.session == sess)
            || st.pending.iter().any(|t| t.session == sess)
        {
            return;
        }
        let founder = st.peers.iter().find_map(|((n, g), e)| {
            (*n == sess)
                .then_some(e.state.measurement_endpoint.map(|m| (m, *g)))
                .flatten()
        });
        let target = founder.or_else(|| {
            st.peers.iter().find_map(|((_, g), e)| {
                (e.state.session == Some(sess))
                    .then_some(e.state.measurement_endpoint.map(|m| (m, *g)))
                    .flatten()
            })
        });
        let Some((target, gateway)) = target else {
            return;
        };
        st.pending.push_back(MeasureTask {
            session: sess,
            target,
            gateway,
            purpose: Purpose::Remeasure,
        });
    }
}

/// Median of the collected samples, rounded to µs (chapter 02 §5).
fn median_round(samples: &mut [f64]) -> i64 {
    samples.sort_by(|a, b| a.partial_cmp(b).expect("no NaN samples"));
    let n = samples.len();
    let median = if n % 2 == 1 {
        samples[n / 2]
    } else {
        (samples[n / 2 - 1] + samples[n / 2]) / 2.0
    };
    median.round() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn median_is_robust_to_outliers() {
        let mut s = vec![10.0, 11.0, 9.0, 10.5, 500_000.0];
        assert_eq!(median_round(&mut s), 11); // median 10.5, rounded
        let mut s = vec![10.0, 12.0];
        assert_eq!(median_round(&mut s), 11);
    }

    /// §4.2: the 5-retry budget is cumulative over a measurement's lifetime.
    /// A pong re-arms the 50 ms timer but never restores the budget, and the
    /// first expiry after the budget is spent fails the measurement.
    #[test]
    fn pong_rearms_retry_timer_but_never_restores_budget() {
        let eng = Engine::new(
            120.0,
            crate::Config {
                gateways: vec![],
                ..crate::Config::default()
            },
        );
        let mut st = eng.lock();
        st.enabled = true;
        let sess = Id(*b"SESSIONX");
        let target: SocketAddr = "127.0.0.1:9999".parse().unwrap();
        st.measurement = Some(Measurement {
            task: MeasureTask {
                session: sess,
                target,
                gateway: 0,
                purpose: Purpose::Join,
            },
            samples: Vec::new(),
            timeouts: 3,
            retry_deadline: 0,
        });

        // A valid pong from the measured endpoint re-arms the timer only.
        let pong = sync::encode_pong(sess, 1, &[]);
        eng.handle_measurement_datagram(&mut st, 0, target, &pong);
        let m = st.measurement.as_ref().expect("measurement still running");
        assert_eq!(m.timeouts, 3, "pong receipt must not restore the budget");
        assert!(m.retry_deadline > 0, "pong receipt re-arms the timer");

        // Two more expiries spend the budget (4, 5); the next one fails.
        for spent in [4, 5] {
            st.measurement.as_mut().unwrap().retry_deadline = 0;
            eng.housekeeping_pass(&mut st);
            assert_eq!(st.measurement.as_ref().unwrap().timeouts, spent);
        }
        st.measurement.as_mut().unwrap().retry_deadline = 0;
        eng.housekeeping_pass(&mut st);
        assert!(
            st.measurement.is_none(),
            "expiry after the 5-retry budget fails the measurement"
        );
    }
}
