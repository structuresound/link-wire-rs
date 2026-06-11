//! Conformance candidate: exposes a peer through the line-based
//! stdin/stdout interface of the spec repo's CANDIDATE-CONTRACT.md.
//!
//! Starts disabled, tempo 120 bpm, quantum 4. With the audio feature it
//! publishes exactly one channel on `audio-enable` and pumps a continuous
//! sine into it so subscribers receive audio.

use std::io::BufRead;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tactus::Link;

const QUANTUM: f64 = 4.0;
const SAMPLE_RATE: u32 = 48_000;
const CHANNEL_NAME: &str = "tactus";

#[derive(Default)]
struct AudioCtl {
    published: Option<[u8; 8]>,
    subscribed: Option<[u8; 8]>,
}

fn main() {
    let link = Arc::new(Link::new(120.0));
    let audio = Arc::new(Mutex::new(AudioCtl::default()));

    // Status emitter + audio pump.
    spawn_status_and_pump(link.clone(), audio.clone());

    println!("ready");

    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let mut words = line.split_whitespace();
        match words.next() {
            Some("enable") => link.enable(),
            Some("disable") => link.disable(),
            Some("tempo") => {
                if let Some(bpm) = words.next().and_then(|w| w.parse::<f64>().ok()) {
                    link.set_tempo(bpm);
                }
            }
            Some("start") => link.set_is_playing(true),
            Some("stop") => link.set_is_playing(false),
            Some("startstop-sync") => {
                if let Some(v) = words.next() {
                    link.enable_start_stop_sync(v == "1");
                }
            }
            Some("audio-enable") => {
                link.enable_audio("tactus-peer");
                let mut ctl = audio.lock().unwrap();
                if ctl.published.is_none() {
                    ctl.published = link.publish_channel(CHANNEL_NAME);
                }
            }
            Some("audio-disable") => {
                let mut ctl = audio.lock().unwrap();
                ctl.published = None;
                ctl.subscribed = None;
                link.disable_audio();
            }
            Some("audio-subscribe") => {
                let index: usize = words.next().and_then(|w| w.parse().ok()).unwrap_or(0);
                let channels = link.visible_channels();
                if let Some(ch) = channels.get(index) {
                    let mut ctl = audio.lock().unwrap();
                    if let Some(old) = ctl.subscribed.take() {
                        link.unsubscribe_channel(old);
                    }
                    link.subscribe_channel(ch.id, QUANTUM);
                    ctl.subscribed = Some(ch.id);
                }
            }
            Some("audio-unsubscribe") => {
                let mut ctl = audio.lock().unwrap();
                if let Some(id) = ctl.subscribed.take() {
                    link.unsubscribe_channel(id);
                }
            }
            Some("quit") => break,
            _ => {} // unknown commands are ignored per the contract
        }
    }
    // Announce departure (channel byes, then ByeBye) before exiting; the
    // pump thread keeps an Arc alive, so Drop would not run.
    link.disable();
    std::process::exit(0);
}

fn spawn_status_and_pump(link: Arc<Link>, audio: Arc<Mutex<AudioCtl>>) {
    std::thread::spawn(move || {
        let started = Instant::now();
        let mut generated_frames: u64 = 0;
        let mut cursor_beat: Option<f64> = None;
        let mut phase: f64 = 0.0;
        let mut last_status = Instant::now() - Duration::from_secs(1);
        let mut last_line = String::new();
        loop {
            // ---- audio pump: keep the sink fed in real time.
            let published = audio.lock().unwrap().published;
            if let Some(channel) = published {
                let target = (started.elapsed().as_secs_f64() * SAMPLE_RATE as f64) as u64;
                let frames = (target - generated_frames).min(SAMPLE_RATE as u64 / 10);
                generated_frames = target;
                if frames > 0 {
                    let begin = cursor_beat
                        .unwrap_or_else(|| link.beat_at_time(link.clock_micros(), QUANTUM));
                    let samples: Vec<i16> = (0..frames)
                        .map(|_| {
                            phase += 440.0 * std::f64::consts::TAU / SAMPLE_RATE as f64;
                            (phase.sin() * 8000.0) as i16
                        })
                        .collect();
                    link.write_channel(channel, &samples, SAMPLE_RATE, 1, begin, QUANTUM);
                    let beats = frames as f64 / SAMPLE_RATE as f64 * (link.tempo() / 60.0);
                    cursor_beat = Some(begin + beats);
                }
            } else {
                cursor_beat = None;
                generated_frames = (started.elapsed().as_secs_f64() * SAMPLE_RATE as f64) as u64;
            }
            // Keep the subscription inbox drained.
            let subscribed = audio.lock().unwrap().subscribed;
            if let Some(id) = subscribed {
                let _ = link.poll_channel(id);
            }

            // ---- status line: on change and at least every 400 ms.
            let beat = link.beat_at_time(link.clock_micros(), QUANTUM);
            let receiving = subscribed.is_some_and(|id| link.is_receiving(id));
            let publishing = published.is_some_and(|id| link.has_requesters(id));
            let line = format!(
                "status peers={} tempo={:.2} playing={} beat={:.4} quantum={} \
                 audio={} channels={} receiving={} publishing={}",
                link.num_peers(),
                link.tempo(),
                link.is_playing() as u8,
                beat,
                QUANTUM,
                link.is_audio_enabled() as u8,
                link.visible_channels().len(),
                receiving as u8,
                publishing as u8,
            );
            // `beat` always changes; compare everything else.
            let key = line.split(" beat=").next().unwrap_or("").to_string()
                + line.split(" quantum=").nth(1).unwrap_or("");
            if key != last_line || last_status.elapsed() >= Duration::from_millis(400) {
                println!("{line}");
                last_line = key;
                last_status = Instant::now();
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    });
}
