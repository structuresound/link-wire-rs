//! M3 acceptance (self-interop on loopback): channel announce/request
//! lifecycle, PCM i16 streaming with beat-time alignment, unsubscribe and
//! withdrawal (spec chapter 03). Reference interop is M4.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use tactus::Link;

// Serialize against the discovery port like the other loopback tests; both
// test binaries can run concurrently only because cargo runs them in
// separate processes — the in-process tests still serialize here.
static NET: Mutex<()> = Mutex::new(());

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

#[test]
fn audio_channel_lifecycle() {
    let _net = NET.lock().unwrap_or_else(|e| e.into_inner());

    let alice = Link::new(120.0);
    alice.enable();
    std::thread::sleep(Duration::from_millis(700));
    let bob = Link::new(120.0);
    bob.enable();

    wait_for("session", Duration::from_secs(10), || {
        alice.num_peers() == 1 && alice.session_id() == bob.session_id()
    });

    alice.enable_audio("Alice");
    bob.enable_audio("Bob");
    let channel = alice.publish_channel("A Sink").expect("audio enabled");

    // Bob sees Alice's channel through unicast announcements (ch. 03 §4.1).
    wait_for("channel visible to Bob", Duration::from_secs(10), || {
        bob.visible_channels()
            .iter()
            .any(|c| c.id == channel && c.name == "A Sink" && c.peer_name == "Alice")
    });

    // Bob subscribes; Alice gains a requester (ch. 03 §4.3, §7.2).
    bob.subscribe_channel(channel, 4.0);
    wait_for("Alice sees the request", Duration::from_secs(5), || {
        alice.has_requesters(channel)
    });

    // Alice streams a beat-stamped ramp; Bob must receive it (ch. 03 §5).
    let sample_rate = 48_000u32;
    let mut sent: Vec<i16> = Vec::new();
    let mut cursor_beat = alice.beat_at_time(alice.clock_micros(), 4.0);
    let beats_per_chunk = |frames: f64| frames / sample_rate as f64 * (alice.tempo() / 60.0);
    let mut value: i16 = 0;
    for _ in 0..40 {
        let chunk: Vec<i16> = (0..480)
            .map(|_| {
                value = value.wrapping_add(7);
                value
            })
            .collect();
        alice.write_channel(channel, &chunk, sample_rate, 1, cursor_beat, 4.0);
        cursor_beat += beats_per_chunk(480.0);
        sent.extend(chunk);
        std::thread::sleep(Duration::from_millis(10));
    }

    wait_for("Bob is receiving", Duration::from_secs(5), || {
        bob.is_receiving(channel)
    });

    let chunks = bob.poll_channel(channel);
    assert!(!chunks.is_empty(), "no chunks delivered");
    // Sample integrity: the received stream is a contiguous slice of what
    // was sent (PCM i16 survives the big-endian wire encoding).
    let received: Vec<i16> = chunks.iter().flat_map(|c| c.samples.clone()).collect();
    let start = sent
        .windows(received.len().min(64))
        .position(|w| w == &received[..w.len()])
        .expect("received samples are a slice of the sent stream");

    // Beat alignment (ch. 03 §6): wire beats are app beats minus the
    // sender's session offset; the receiver adds its own. With one shared
    // session timeline the round trip is pure integer beat arithmetic, so
    // the received chunk's beat must equal Alice's send cursor at the same
    // stream offset almost exactly.
    let first = &chunks[0];
    assert_eq!(first.sample_rate, sample_rate);
    assert_eq!(first.channels, 1);
    assert_eq!(first.tempo_micros_per_beat, 500_000);
    let stream_start_beat = cursor_beat - beats_per_chunk(40.0 * 480.0);
    let alice_first_beat = stream_start_beat + beats_per_chunk(start as f64);
    let diff = (first.begin_app_beat - alice_first_beat).abs();
    assert!(
        diff < 0.001,
        "beat misalignment: alice {alice_first_beat} vs received {}",
        first.begin_app_beat
    );

    // Sequence numbers start at 1 and increase (ch. 03 §5.3).
    assert!(chunks[0].seq >= 1);
    let seqs: Vec<u64> = chunks.iter().map(|c| c.seq).collect();
    assert!(seqs.windows(2).all(|w| w[1] > w[0]), "seqs not increasing");

    // Unsubscribe stops the flow immediately (ch. 03 §4.3).
    bob.unsubscribe_channel(channel);
    wait_for("Alice loses the requester", Duration::from_secs(3), || {
        !alice.has_requesters(channel)
    });

    // Withdrawing the channel empties Bob's list (ch. 03 §4.4).
    alice.unpublish_channel(channel);
    wait_for("channel gone from Bob", Duration::from_secs(5), || {
        bob.visible_channels().is_empty()
    });

    // Disabling audio clears the endpoint advertisement (ch. 03 §2).
    alice.disable_audio();
    assert!(!alice.is_audio_enabled());
}

#[test]
fn request_expiry_silences_sink() {
    let _net = NET.lock().unwrap_or_else(|e| e.into_inner());

    let a = Link::new(120.0);
    a.enable();
    std::thread::sleep(Duration::from_millis(700));
    let b = Link::new(120.0);
    b.enable();
    wait_for("session", Duration::from_secs(10), || {
        a.num_peers() == 1 && a.session_id() == b.session_id()
    });
    a.enable_audio("A");
    b.enable_audio("B");
    let channel = a.publish_channel("ch").unwrap();
    wait_for("visible", Duration::from_secs(10), || {
        b.visible_channels().iter().any(|c| c.id == channel)
    });
    b.subscribe_channel(channel, 4.0);
    wait_for("requested", Duration::from_secs(5), || {
        a.has_requesters(channel)
    });

    // B stops refreshing (simulated crash, no StopChannelRequest): the
    // request must expire at ttl + 1 s padding (ch. 03 §7.2), silencing A.
    b.simulate_crash();
    wait_for("request expires", Duration::from_secs(10), || {
        !a.has_requesters(channel)
    });
}
