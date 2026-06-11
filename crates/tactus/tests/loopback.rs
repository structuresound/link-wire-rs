//! M2 acceptance: two peers on the loopback gateway discover each other,
//! elect one session, converge tempo, align beat phase, propagate
//! start/stop, and handle churn (chapter 01–02 behavior, self-interop).
//!
//! Reference interop on top of the same scenarios is M4 (the spec repo's
//! conformance harness).
//!
//! These tests share the real discovery port (20808), so they serialize on
//! a process-wide lock.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use tactus::Link;

static NET: Mutex<()> = Mutex::new(());

fn lock_net() -> std::sync::MutexGuard<'static, ()> {
    NET.lock().unwrap_or_else(|e| e.into_inner())
}

fn wait_for(what: &str, timeout: Duration, mut pred: impl FnMut() -> bool) {
    let t0 = Instant::now();
    while t0.elapsed() < timeout {
        if pred() {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("timed out waiting for {what}");
}

/// Circular distance between two phases at quantum `q`.
fn phase_distance(a: f64, b: f64, q: f64) -> f64 {
    let d = (a - b).rem_euclid(q);
    d.min(q - d)
}

#[test]
fn two_peers_form_one_session_and_converge() {
    let _net = lock_net();

    let a = Link::new(120.0);
    a.enable();
    // A founds its session strictly earlier than B so the join rule
    // (ghost-time seniority, chapter 02 §7.2) sends B into A's session.
    std::thread::sleep(Duration::from_millis(700));
    let b = Link::new(120.0);
    b.enable();

    wait_for("mutual discovery", Duration::from_secs(10), || {
        a.num_peers() == 1 && b.num_peers() == 1
    });
    wait_for("session election", Duration::from_secs(10), || {
        a.session_id() == b.session_id()
    });
    assert_eq!(
        a.session_id(),
        a.node_id(),
        "the older peer should keep its own session"
    );

    // Tempo set on the joiner is adopted by the founder...
    b.set_tempo(140.0);
    wait_for("tempo follows B→A", Duration::from_secs(5), || {
        (a.tempo() - 140.0).abs() < 0.01
    });
    // ...and vice versa.
    a.set_tempo(96.0);
    wait_for("tempo follows A→B", Duration::from_secs(5), || {
        (b.tempo() - 96.0).abs() < 0.01
    });

    // Beat phase at quantum 4 aligns across peers (chapter 02 §9). The two
    // clock reads happen back to back; allow generous slack for scheduling.
    wait_for("phase alignment", Duration::from_secs(5), || {
        let pa = a.phase_at_time(a.clock_micros(), 4.0);
        let pb = b.phase_at_time(b.clock_micros(), 4.0);
        phase_distance(pa, pb, 4.0) < 0.1
    });

    // Start/stop propagates with sync enabled (chapter 02 §8).
    a.enable_start_stop_sync(true);
    b.enable_start_stop_sync(true);
    a.set_is_playing(true);
    wait_for("transport start propagates", Duration::from_secs(5), || {
        b.is_playing()
    });
    b.set_is_playing(false);
    wait_for("transport stop propagates", Duration::from_secs(5), || {
        !a.is_playing()
    });
}

#[test]
fn peer_departure_resets_survivor_session() {
    let _net = lock_net();

    let a = Link::new(120.0);
    a.enable();
    std::thread::sleep(Duration::from_millis(700));
    let b = Link::new(120.0);
    b.enable();

    wait_for("mutual discovery", Duration::from_secs(10), || {
        a.num_peers() == 1 && b.num_peers() == 1
    });
    wait_for("session election", Duration::from_secs(10), || {
        a.session_id() == b.session_id()
    });

    let a_node_before = a.node_id();
    let a_tempo_before = a.tempo();

    // B leaves; its ByeBye removes it immediately, and losing the last
    // session peer makes A found a fresh session under a new NodeId
    // (chapter 01 §7, chapter 02 §7.3).
    drop(b);
    wait_for(
        "survivor forgets departed peer",
        Duration::from_secs(8),
        || a.num_peers() == 0,
    );
    wait_for(
        "survivor founds fresh session",
        Duration::from_secs(8),
        || a.node_id() != a_node_before,
    );
    assert_eq!(a.session_id(), a.node_id());
    // Tempo continues seamlessly across the reset.
    assert!((a.tempo() - a_tempo_before).abs() < 0.01);
}

#[test]
fn ttl_expiry_prunes_silent_peer() {
    let _net = lock_net();

    let a = Link::new(120.0);
    a.enable();
    std::thread::sleep(Duration::from_millis(700));
    let b = Link::new(120.0);
    b.enable();

    wait_for("mutual discovery", Duration::from_secs(10), || {
        a.num_peers() == 1 && b.num_peers() == 1
    });

    // B goes silent without a ByeBye (a crashed peer). A must prune it by
    // ttl expiry: ttl 5 s + 1 s prune padding + timer latency (ch. 01 §7).
    b.simulate_crash();
    let t0 = Instant::now();
    wait_for("silent peer expires", Duration::from_secs(10), || {
        a.num_peers() == 0
    });
    // It must not be pruned *before* its ttl either.
    assert!(
        t0.elapsed() >= Duration::from_secs(4),
        "peer pruned long before its ttl expired"
    );
}
