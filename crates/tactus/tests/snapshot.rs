//! SessionSnapshot: single-lock consistency and extrapolation exactness.
//!
//! These tests use disabled peers only (no sockets, no shared port), so
//! they are safe to run in parallel with the loopback suites.

use tactus::Link;

#[test]
fn snapshot_agrees_with_getters() {
    let link = Link::new(132.5);
    let snap = link.capture_session(4.0);

    assert!(!snap.enabled);
    assert_eq!(snap.num_peers, 0);
    assert_eq!(snap.quantum, 4.0);
    // The wire timeline stores tempo as integer µs/beat, so compare with
    // the getter (same quantization), not the constructor argument.
    assert_eq!(snap.tempo_bpm, link.tempo());
    assert!((snap.tempo_bpm - 132.5).abs() < 1e-3);
    assert_eq!(snap.is_playing, link.is_playing());
    assert_eq!(snap.session_id, link.session_id());

    // The snapshot's beat/phase must match what the getters report for
    // the capture instant (the timeline is unchanged in between).
    let beat = link.beat_at_time(snap.at_micros, 4.0);
    assert!(
        (snap.beat - beat).abs() < 1e-9,
        "snapshot beat {} != beat_at_time {}",
        snap.beat,
        beat
    );
    let phase = link.phase_at_time(snap.at_micros, 4.0);
    assert!(
        (snap.phase - phase).abs() < 1e-9,
        "snapshot phase {} != phase_at_time {}",
        snap.phase,
        phase
    );
}

#[test]
fn extrapolation_is_exact_while_timeline_unchanged() {
    let link = Link::new(120.0);
    let snap = link.capture_session(4.0);

    // At 120 bpm the timeline advances 2 beats per second. Extrapolating
    // one second ahead must agree with beat_at_time at that instant to
    // within the µbeat quantization of the wire timeline.
    let t = snap.at_micros + 1_000_000;
    let expected = link.beat_at_time(t, 4.0);
    let got = snap.beat_at(t);
    assert!(
        (got - expected).abs() < 1e-5,
        "extrapolated {got} != beat_at_time {expected}"
    );
    assert!((got - snap.beat - 2.0).abs() < 1e-5);

    // Phase stays within [0, quantum) and agrees with the getter.
    let p = snap.phase_at(t);
    assert!((0.0..4.0).contains(&p));
    let expected_phase = link.phase_at_time(t, 4.0);
    assert!(
        (p - expected_phase).abs() < 1e-5,
        "extrapolated phase {p} != phase_at_time {expected_phase}"
    );
}

#[test]
fn extrapolation_superseded_by_tempo_change() {
    let link = Link::new(120.0);
    let stale = link.capture_session(4.0);

    link.set_tempo(240.0);
    let fresh = link.capture_session(4.0);

    assert!((fresh.tempo_bpm - 240.0).abs() < 1e-3);
    // A fresh snapshot tracks the new slope; the stale one keeps the old
    // slope (documented behavior — poll cadence bounds the error window).
    let t = fresh.at_micros + 1_000_000;
    assert!((fresh.beat_at(t) - fresh.beat - 4.0).abs() < 1e-5);
    assert!((stale.beat_at(t) - stale.beat_at(fresh.at_micros) - 2.0).abs() < 1e-5);
}

#[test]
fn phase_at_zero_quantum_is_zero() {
    let link = Link::new(120.0);
    let snap = link.capture_session(0.0);
    assert_eq!(snap.phase, 0.0);
    assert_eq!(snap.phase_at(snap.at_micros + 123_456), 0.0);
}
