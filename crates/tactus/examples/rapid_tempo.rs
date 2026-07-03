// Reproduce the conformance tempo-follow failure pattern: one peer steps
// tempo 100->124 in 24 immediate +1 increments; the other must converge.
use std::time::{Duration, Instant};
use tactus::Link;

fn main() {
    let a = Link::new(120.0);
    a.enable();
    std::thread::sleep(Duration::from_millis(700));
    let b = Link::new(120.0);
    b.enable();
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(10) {
        if a.num_peers() == 1 && b.num_peers() == 1 && a.session_id() == b.session_id() {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    assert_eq!(a.session_id(), b.session_id(), "no common session");

    b.set_tempo(100.0);
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(5) {
        if (a.tempo() - 100.0).abs() < 0.01 {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    println!("a adopted 100: {}", a.tempo());

    // burst: 24 single-bpm steps with no delay, like the hut adapter's keys
    for step in 1..=24 {
        a.set_tempo(100.0 + step as f64);
    }
    println!("a after burst: {}", a.tempo());
    let t0 = Instant::now();
    let mut last = 0.0;
    while t0.elapsed() < Duration::from_secs(5) {
        last = b.tempo();
        if (last - 124.0).abs() < 0.01 {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    println!("b after 5s: {last} (elapsed {:?})", t0.elapsed());
    assert!((last - 124.0).abs() < 0.01, "B STUCK AT {last}");
    println!("OK");
}
