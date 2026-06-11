//! A tempo/beat session peer wire-compatible with the Link protocol family,
//! implemented clean-room from the published specification
//! ([link-wire-spec](https://github.com/structuresound/link-wire-spec)).
//!
//! ```no_run
//! let link = tactus::Link::new(120.0);
//! link.enable();
//! let beat = link.beat_at_time(link.clock_micros(), 4.0);
//! ```
//!
//! Current scope: IPv4 gateways, loopback by default (configure others via
//! [`Config::gateways`]); interface enumeration and IPv6 link-local gateways
//! are not implemented yet.

mod audio;
mod engine;
pub mod math;
mod net;
mod runtime;

use std::net::Ipv4Addr;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use engine::Engine;
pub use tactus_wire as wire;

/// Peer configuration.
#[derive(Debug, Clone)]
pub struct Config {
    /// Local interface addresses to run gateways on (chapter 00 §2). Each
    /// address gets its own discovery/measurement/audio sockets.
    pub gateways: Vec<Ipv4Addr>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            gateways: vec![Ipv4Addr::LOCALHOST],
        }
    }
}

/// A session peer. Starts **disabled**; call [`Link::enable`] to join the
/// network. Dropping the handle announces departure.
pub struct Link {
    eng: Arc<Engine>,
}

impl Link {
    /// Create a disabled peer with the given initial tempo.
    pub fn new(bpm: f64) -> Link {
        Link::with_config(bpm, Config::default())
    }

    pub fn with_config(bpm: f64, config: Config) -> Link {
        let eng = Arc::new(Engine::new(bpm, config));
        runtime::spawn_housekeeping(eng.clone());
        Link { eng }
    }

    /// Microseconds on this peer's local clock (the time base of every
    /// `*_micros` parameter below).
    pub fn clock_micros(&self) -> i64 {
        self.eng.now()
    }

    pub fn enable(&self) {
        runtime::enable(&self.eng);
    }

    pub fn disable(&self) {
        runtime::disable(&self.eng);
    }

    pub fn is_enabled(&self) -> bool {
        self.eng.lock().enabled
    }

    /// Number of other peers in the current session.
    pub fn num_peers(&self) -> usize {
        let st = self.eng.lock();
        if !st.enabled {
            return 0;
        }
        let mut nodes: Vec<_> = st
            .peers
            .iter()
            .filter(|(_, e)| e.state.session == Some(st.session))
            .map(|((n, _), _)| *n)
            .collect();
        nodes.sort();
        nodes.dedup();
        nodes.len()
    }

    /// Session tempo in bpm.
    pub fn tempo(&self) -> f64 {
        math::tempo_to_bpm(self.eng.lock().timeline.tempo)
    }

    /// Set the session tempo (clamped to 20–999 bpm). Emits a timeline with
    /// a strictly increased beat origin (chapter 02 §6 rule 3) and gossips
    /// it immediately.
    pub fn set_tempo(&self, bpm: f64) {
        let mut st = self.eng.lock();
        let now = self.eng.now();
        let beats_now = math::beats_at_ghost(&st.timeline, now + st.ghost_offset);
        let beat_origin = beats_now.max(st.timeline.beat_origin + 1);
        let time_origin = math::ghost_at_beats(&st.timeline, beat_origin);
        st.timeline = wire::types::Timeline {
            tempo: math::bpm_to_tempo(bpm),
            beat_origin,
            time_origin,
        };
        self.eng.schedule_broadcast(&mut st);
    }

    /// The application beat value for local time `micros` at `quantum`
    /// (chapter 02 §9).
    pub fn beat_at_time(&self, micros: i64, quantum: f64) -> f64 {
        let st = self.eng.lock();
        let q = quantum_micro_beats(quantum);
        math::app_beat_at_ghost(&st.timeline, micros + st.ghost_offset, q) as f64 / 1e6
    }

    /// The phase within `quantum` at local time `micros`, in `[0, quantum)`.
    pub fn phase_at_time(&self, micros: i64, quantum: f64) -> f64 {
        let st = self.eng.lock();
        let q = quantum_micro_beats(quantum);
        let b = math::app_beat_at_ghost(&st.timeline, micros + st.ghost_offset, q);
        math::phase(b, q) as f64 / 1e6
    }

    /// The local time at which application beat `beat` falls (inverse of
    /// [`Link::beat_at_time`], with the composing tie-break of ch. 02 §9).
    pub fn time_at_beat(&self, beat: f64, quantum: f64) -> i64 {
        let st = self.eng.lock();
        let q = quantum_micro_beats(quantum);
        let b = (beat * 1e6).round() as i64;
        math::ghost_at_app_beat(&st.timeline, b, q) - st.ghost_offset
    }

    /// The transport state visible to the application.
    pub fn is_playing(&self) -> bool {
        self.eng.lock().app_playing
    }

    /// Start/stop the transport. With start/stop sync enabled this gossips
    /// a new `stst` (chapter 02 §8); otherwise it is local only.
    pub fn set_is_playing(&self, playing: bool) {
        let mut st = self.eng.lock();
        st.app_playing = playing;
        if st.sst_sync {
            let ghost = self.eng.now() + st.ghost_offset;
            st.held_stst = wire::types::StartStopState {
                is_playing: playing,
                beats: math::beats_at_ghost(&st.timeline, ghost),
                // Strictly-greater rule: never reuse a timestamp (§8 rule 2).
                timestamp: ghost.max(st.held_stst.timestamp + 1),
            };
            self.eng.schedule_broadcast(&mut st);
        }
    }

    pub fn is_start_stop_sync_enabled(&self) -> bool {
        self.eng.lock().sst_sync
    }

    pub fn enable_start_stop_sync(&self, enabled: bool) {
        let mut st = self.eng.lock();
        st.sst_sync = enabled;
        if enabled && st.held_stst != wire::types::StartStopState::default() {
            st.app_playing = st.held_stst.is_playing;
        }
    }

    /// Test hook: go silent *without* a ByeBye, as a crashed peer would,
    /// so ttl-expiry behavior (chapter 01 §7) can be exercised.
    #[doc(hidden)]
    pub fn simulate_crash(&self) {
        let mut st = self.eng.lock();
        if !st.enabled {
            return;
        }
        st.enabled = false;
        st.epoch += 1;
        st.gateways.clear();
        st.peers.clear();
        st.measurement = None;
        st.pending.clear();
        st.broadcast_due = None;
        self.eng.notify();
    }

    /// This peer's node identifier (changes on enable and on session reset).
    pub fn node_id(&self) -> [u8; 8] {
        self.eng.lock().node.0
    }

    /// The current session identifier (the founding peer's node id).
    pub fn session_id(&self) -> [u8; 8] {
        self.eng.lock().session.0
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        runtime::disable(&self.eng);
        self.eng.shutdown.store(true, Ordering::Relaxed);
        self.eng.notify();
    }
}

fn quantum_micro_beats(quantum: f64) -> i64 {
    if quantum <= 0.0 {
        0
    } else {
        (quantum * 1e6).round() as i64
    }
}
