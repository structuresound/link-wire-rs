//! Timeline and beat-grid arithmetic (spec chapter 02 §2, §6, §9).
//!
//! All beat values are micro-beats (`i64`), all times microseconds (`i64`),
//! exactly as on the wire; intermediate products use `i128` to avoid
//! overflow. Division rounds to nearest per chapter 02 §6.

use tactus_wire::types::Timeline;

/// Tempo clamp bounds (chapter 02 §6 rule 1), as wire periods. 999 bpm is
/// the *fastest* tempo and therefore the smallest period.
pub const TEMPO_MIN_MICROS_PER_BEAT: i64 = 60_060; // round(60e6 / 999)
pub const TEMPO_MAX_MICROS_PER_BEAT: i64 = 3_000_000; // round(60e6 / 20)

/// Division rounding to nearest (ties away from zero); denominator > 0.
pub fn round_div(num: i128, den: i128) -> i64 {
    debug_assert!(den > 0);
    let q = if num >= 0 {
        (num + den / 2) / den
    } else {
        (num - den / 2) / den
    };
    q as i64
}

/// `phase(b, q)`: `b mod q` shifted into `[0, q)`; `phase(b, 0) = 0`
/// (chapter 02 §9).
pub fn phase(b: i64, q: i64) -> i64 {
    if q <= 0 {
        return 0;
    }
    b.rem_euclid(q)
}

/// `alignUp(x, t, q)`: least value ≥ `x` having the phase of `t`
/// (chapter 02 §9).
pub fn align_up(x: i64, t: i64, q: i64) -> i64 {
    if q <= 0 {
        return x;
    }
    x + (phase(t, q) - phase(x, q)).rem_euclid(q)
}

/// `alignNear(x, t, q)`: the value with `t`'s phase nearest to `x`
/// (deviation ≤ q/2, ties at exactly q/2 resolving downward); defined as
/// `alignUp(x − q/2, t, q)` (chapter 02 §9).
pub fn align_near(x: i64, t: i64, q: i64) -> i64 {
    if q <= 0 {
        return x;
    }
    align_up(x - q / 2, t, q)
}

/// Clamp a received or requested tempo into the 20–999 bpm range
/// (chapter 02 §6 rule 1; receivers re-clamp after decoding).
pub fn clamp_tempo(micros_per_beat: i64) -> i64 {
    micros_per_beat.clamp(TEMPO_MIN_MICROS_PER_BEAT, TEMPO_MAX_MICROS_PER_BEAT)
}

/// bpm → wire tempo: round(60 × 10⁶ / bpm) (chapter 00 §4.7).
pub fn bpm_to_tempo(bpm: f64) -> i64 {
    clamp_tempo((60_000_000.0 / bpm.clamp(20.0, 999.0)).round() as i64)
}

/// Wire tempo → bpm.
pub fn tempo_to_bpm(micros_per_beat: i64) -> f64 {
    60_000_000.0 / micros_per_beat as f64
}

/// `beats(g)` of the timeline bijection (chapter 02 §6): ghost µs → µbeats.
pub fn beats_at_ghost(tl: &Timeline, ghost: i64) -> i64 {
    tl.beat_origin
        + round_div(
            (ghost - tl.time_origin) as i128 * 1_000_000,
            tl.tempo as i128,
        )
}

/// `ghost(b)` of the timeline bijection (chapter 02 §6): µbeats → ghost µs.
pub fn ghost_at_beats(tl: &Timeline, beats: i64) -> i64 {
    tl.time_origin
        + round_div(
            (beats - tl.beat_origin) as i128 * tl.tempo as i128,
            1_000_000,
        )
}

/// The beat value reported to the application for ghost time `g` at quantum
/// `q` (µbeats): `alignNear(B, B − beatOrigin, q)` (chapter 02 §9).
pub fn app_beat_at_ghost(tl: &Timeline, ghost: i64, q: i64) -> i64 {
    let b = beats_at_ghost(tl, ghost);
    align_near(b, b - tl.beat_origin, q)
}

/// Inverse of [`app_beat_at_ghost`]: application beat → ghost time, with the
/// opposite tie-break so the two directions compose (chapter 02 §9 [N]).
pub fn ghost_at_app_beat(tl: &Timeline, b: i64, q: i64) -> i64 {
    if q <= 0 {
        return ghost_at_beats(tl, b);
    }
    let r = b - tl.beat_origin;
    let cycle = r - phase(r, q);
    let delta = align_near(q - phase(r, q), q - phase(b, q), q);
    ghost_at_beats(tl, tl.beat_origin + cycle + q - delta)
}

/// A peer's session offset Δ for quantum `q` (chapter 03 §6.2):
/// `localBeats − Δ` is the origin-independent session beat time.
pub fn session_offset(tl: &Timeline, q: i64) -> i64 {
    let b0 = beats_at_ghost(tl, tl.time_origin); // == beat_origin
    align_near(b0, b0 - tl.beat_origin, q)
}

#[cfg(test)]
mod tests {
    use super::*;

    const Q: i64 = 4_000_000; // quantum 4 in µbeats

    fn tl(tempo: i64, beat_origin: i64, time_origin: i64) -> Timeline {
        Timeline {
            tempo,
            beat_origin,
            time_origin,
        }
    }

    #[test]
    fn phase_handles_negatives() {
        assert_eq!(phase(7, 4), 3);
        assert_eq!(phase(-1, 4), 3);
        assert_eq!(phase(-9, 4), 3);
        assert_eq!(phase(123, 0), 0);
    }

    #[test]
    fn align_up_is_least_value_with_target_phase() {
        for x in -20..20 {
            for t in -20..20 {
                let a = align_up(x, t, 4);
                assert!(a >= x && a < x + 4);
                assert_eq!(phase(a, 4), phase(t, 4));
            }
        }
    }

    #[test]
    fn align_near_deviates_at_most_half_quantum() {
        for x in -20..20 {
            for t in -20..20 {
                let a = align_near(x, t, 4);
                assert!((a - x).abs() <= 2);
                assert_eq!(phase(a, 4), phase(t, 4));
                // Tie at exactly q/2 resolves downward.
                if (a - x).abs() == 2 {
                    assert_eq!(a, x - 2);
                }
            }
        }
    }

    #[test]
    fn timeline_bijection_examples() {
        // 120 bpm, beat origin 0 at ghost 0: one beat each 500 ms.
        let t = tl(500_000, 0, 0);
        assert_eq!(beats_at_ghost(&t, 1_000_000), 2_000_000);
        assert_eq!(ghost_at_beats(&t, 2_000_000), 1_000_000);
        // Rounding to nearest.
        assert_eq!(beats_at_ghost(&t, 1), 2);
        // Offset origins.
        let t = tl(500_000, 3_000_000, 10_000_000);
        assert_eq!(beats_at_ghost(&t, 10_500_000), 4_000_000);
        assert_eq!(ghost_at_beats(&t, 4_000_000), 10_500_000);
    }

    #[test]
    fn timeline_bijection_no_overflow_at_large_values() {
        // Hours of ghost time at extreme tempo must not overflow.
        let t = tl(TEMPO_MIN_MICROS_PER_BEAT, 0, 0);
        let g = 100i64 * 3600 * 1_000_000; // 100 hours
        let b = beats_at_ghost(&t, g);
        assert!((ghost_at_beats(&t, b) - g).abs() <= 1);
    }

    #[test]
    fn app_beat_phase_encodes_against_quantum() {
        let t = tl(500_000, 0, 0);
        // At ghost 0 (beat 0) the app beat is 0 for any quantum.
        assert_eq!(app_beat_at_ghost(&t, 0, Q), 0);
        // The app beat always carries the phase of the session beat grid.
        for g in (0..20_000_000).step_by(333_333) {
            let b_app = app_beat_at_ghost(&t, g, Q);
            let b_raw = beats_at_ghost(&t, g);
            assert_eq!(phase(b_app, Q), phase(b_raw - t.beat_origin, Q));
            assert!((b_app - b_raw).abs() <= Q / 2);
        }
    }

    #[test]
    fn beat_time_directions_compose() {
        // chapter 02 §9: the inverse must use the opposite tie-break so
        // b → t → b is the identity.
        let t = tl(495_868, 7_345_678, 12_345_678);
        for i in -50..50 {
            let b = i * 567_891;
            let g = ghost_at_app_beat(&t, b, Q);
            let b2 = app_beat_at_ghost(&t, g, Q);
            // Identity up to the µbeat rounding of the two conversions.
            assert!((b2 - b).abs() <= 1, "b={b} g={g} b2={b2}");
        }
    }

    #[test]
    fn tempo_conversions_match_spec_examples() {
        assert_eq!(bpm_to_tempo(120.0), 500_000); // chapter 00 §4.7
        assert_eq!(bpm_to_tempo(999.0), 60_060); // chapter 02 §6 rule 1
        assert_eq!(bpm_to_tempo(5000.0), 60_060); // clamped
        assert_eq!(bpm_to_tempo(1.0), 3_000_000); // clamped
        assert!((tempo_to_bpm(500_000) - 120.0).abs() < 1e-9);
    }

    #[test]
    fn session_beat_time_is_quantum_independent() {
        // chapter 03 §6.2: localBeats − Δ is the origin-independent session
        // beat time. Since the app beat is align-near-encoded against the
        // quantum and Δ is the same encoding of the beat origin, the
        // difference collapses to beats-since-origin — for *any* quantum.
        let t = tl(495_868, 7_345_678, 12_345_678);
        for g in (0..30_000_000).step_by(1_234_567) {
            let raw = beats_at_ghost(&t, g) - t.beat_origin;
            for q in [1_000_000, Q, 3_000_000, 8_000_000] {
                let s = app_beat_at_ghost(&t, g, q) - session_offset(&t, q);
                assert_eq!(s, raw, "g={g} q={q}");
            }
        }
    }
}
