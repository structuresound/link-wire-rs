//! LinkAudio v1 runtime (spec chapter 03): channel announce/request
//! lifecycle, PCM i16 sink and source, beat-time-aligned scheduling.

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::Arc;

use tactus_wire::audio as wire;
use tactus_wire::types::{ChannelId, NodeId, SessionId, Timeline};

use crate::engine::{random_id, Engine, PeerEntry, State};
use crate::math;
use crate::net::Gateway;

/// A source is "receiving" while buffers arrived within this window.
const RECEIVING_WINDOW: i64 = 1_000_000;
/// Announcement nominal period (chapter 03 §4.1).
const ANNOUNCE_PERIOD: i64 = 250_000;
/// Source re-request period (chapter 03 §4.3).
const REQUEST_PERIOD: i64 = 5_000_000;
/// Prune-timer padding for requesters and channels (chapter 03 §7).
const PRUNE_PADDING: i64 = 1_000_000;
/// RTT sliding-window length (chapter 03 §4.2).
const RTT_WINDOW: usize = 10;
/// Cap on undelivered chunks buffered per source.
const INBOX_LIMIT: usize = 256;

type Peers = HashMap<(NodeId, usize), PeerEntry>;
type Paths = HashMap<(NodeId, usize), PathStats>;

/// Per-peer LinkAudio runtime state. Present iff LinkAudio is enabled, which
/// is also what switches the `aep4` advertisement on (chapter 03 §2).
pub struct AudioState {
    pub peer_name: Vec<u8>,
    pub sinks: Vec<Sink>,
    pub sources: HashMap<ChannelId, Source>,
    /// Remote channels, tracked per (channel id, gateway) (chapter 03 §7.3).
    pub known: HashMap<(ChannelId, usize), KnownChannel>,
    /// RTT path metrics per (peer, gateway) (chapter 03 §4.2).
    pub paths: Paths,
    pub last_announce_at: i64,
}

impl AudioState {
    fn new(peer_name: &str) -> AudioState {
        let mut name = peer_name.as_bytes().to_vec();
        name.truncate(wire::MAX_NAME); // sender-side cap (chapter 03 §8)
        AudioState {
            peer_name: name,
            sinks: Vec::new(),
            sources: HashMap::new(),
            known: HashMap::new(),
            paths: HashMap::new(),
            last_announce_at: 0,
        }
    }
}

pub struct KnownChannel {
    pub peer: NodeId,
    pub peer_name: Vec<u8>,
    pub name: Vec<u8>,
    pub deadline: i64,
}

/// Keepalive RTT window and the derived path quality (chapter 03 §4.2).
#[derive(Default)]
pub struct PathStats {
    rtts: VecDeque<f64>,
}

impl PathStats {
    fn record(&mut self, rtt_micros: f64) {
        if self.rtts.len() == RTT_WINDOW {
            self.rtts.pop_front();
        }
        self.rtts.push_back(rtt_micros);
    }

    fn quality(&self) -> f64 {
        let n = self.rtts.len();
        if n == 0 {
            return 0.0;
        }
        let mean = self.rtts.iter().sum::<f64>() / n as f64;
        let var = self.rtts.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / n as f64;
        // Few samples are penalized by the additive jitter term (§4.2).
        let jitter = var.sqrt() + (1e4 - 1e4 * n as f64 / RTT_WINDOW as f64);
        let speed = 1e6 / mean.max(1.0);
        speed / (1.0 + jitter)
    }
}

/// One pending run of contiguous beat-stamped material on a sink.
struct Segment {
    /// Wire (session) beat time of the first pending frame, µbeats.
    begin: i64,
    /// Tempo of this material, µs/beat.
    tempo: i64,
    samples: Vec<i16>,
}

/// The transmitting end of a published channel (chapter 00 §2).
pub struct Sink {
    pub id: ChannelId,
    pub name: Vec<u8>,
    /// Requesting peer → request expiry deadline (chapter 03 §7.2).
    pub requesters: HashMap<NodeId, i64>,
    /// Next chunk sequence number; the first chunk is 1 (chapter 03 §5.3).
    next_seq: u64,
    segments: VecDeque<Segment>,
    format: Option<(u32, u8)>, // (sample rate, channels)
}

/// The receiving end of a subscribed channel (chapter 00 §2).
pub struct Source {
    pub quantum: i64,
    pub last_request_at: i64,
    pub last_buffer_at: i64,
    pub inbox: VecDeque<ReceivedChunk>,
    /// Next chunk sequence number expected; `None` until the first chunk
    /// arrives (a mid-stream subscription is not counted as loss).
    pub next_seq_expected: Option<u64>,
    /// Chunks detected lost from sequence gaps (chapter 03 §5.8).
    pub lost_chunks: u64,
}

/// One delivered chunk (chapter 03 §7.4): frames plus their position on the
/// local beat grid.
#[derive(Debug, Clone)]
pub struct ReceivedChunk {
    /// Interleaved native-endian samples.
    pub samples: Vec<i16>,
    /// Local application beat of the first frame (`b_wire + Δ_receiver`,
    /// chapter 03 §6.4).
    pub begin_app_beat: f64,
    pub tempo_micros_per_beat: i64,
    pub sample_rate: u32,
    pub channels: u8,
    pub seq: u64,
}

/// Duration of `frames` at `rate`/`tempo`, in µbeats (chapter 03 §5.3).
fn frames_to_micro_beats(frames: i64, rate: u32, tempo: i64) -> i64 {
    math::round_div(
        frames as i128 * 1_000_000_000_000,
        rate as i128 * tempo as i128,
    )
}

// ----------------------------------------------------------- path lookup

/// All (gateway, endpoint) pairs of session peers with an audio endpoint.
fn session_endpoints(peers: &Peers, session: SessionId) -> Vec<(usize, SocketAddr)> {
    peers
        .iter()
        .filter(|(_, e)| e.state.session == Some(session))
        .filter_map(|((_, g), e)| e.state.audio_endpoint.map(|ep| (*g, ep)))
        .collect()
}

/// The best-quality path to a peer (chapter 03 §4.2).
fn best_path(peers: &Peers, paths: &Paths, node: NodeId) -> Option<(usize, SocketAddr)> {
    peers
        .iter()
        .filter(|((n, _), _)| *n == node)
        .filter_map(|((_, g), e)| e.state.audio_endpoint.map(|ep| (*g, ep)))
        .max_by(|(g1, _), (g2, _)| {
            let q1 = paths.get(&(node, *g1)).map_or(0.0, PathStats::quality);
            let q2 = paths.get(&(node, *g2)).map_or(0.0, PathStats::quality);
            q1.total_cmp(&q2)
        })
}

fn send_via(gateways: &[Arc<Gateway>], gw: usize, bytes: &[u8], dst: SocketAddr) {
    if let Some(g) = gateways.get(gw) {
        let _ = g.audio.send_to(bytes, dst);
    }
}

// --------------------------------------------------------------- inbound

pub fn handle_datagram(eng: &Engine, st: &mut State, gw: usize, src: SocketAddr, buf: &[u8]) {
    if !st.enabled || st.audio.is_none() {
        return;
    }
    let Ok(frame) = wire::decode(buf) else { return };
    // Admission rules (chapter 03 §3).
    if frame.node == st.node || frame.group_id != 0 {
        return;
    }
    let now = eng.now();
    let timeline = st.timeline;
    let session = st.session;
    let own_node = st.node;
    let State {
        audio,
        peers,
        gateways,
        ..
    } = st;
    let audio = audio.as_mut().expect("checked above");

    match frame.message {
        wire::Message::PeerAnnouncement {
            peer_name,
            channels,
            ping,
            ..
        } => {
            // Answer the embedded ping regardless of endpoint admission
            // (chapter 03 §4.2, §7.3).
            if let Some(ht) = ping {
                let pong = wire::Frame::new(own_node, wire::Message::Pong { host_time: ht });
                send_via(gateways, gw, &wire::encode(&pong), src);
            }
            // Channel content only from a source endpoint learned through
            // discovery (chapter 03 §7.3).
            let endpoint_known = peers
                .get(&(frame.node, gw))
                .is_some_and(|e| e.state.audio_endpoint == Some(src));
            if !endpoint_known {
                return;
            }
            for ch in channels {
                // Listing implicitly refreshes the channel (chapter 03 §4.1).
                audio.known.insert(
                    (ch.id, gw),
                    KnownChannel {
                        peer: frame.node,
                        peer_name: peer_name.clone(),
                        name: ch.name,
                        deadline: now + frame.ttl as i64 * 1_000_000,
                    },
                );
            }
            eng.notify();
        }
        wire::Message::Pong { host_time } => {
            // RTT measured purely on our own clock (chapter 03 §4.2).
            let rtt = (now - host_time).max(0) as f64;
            audio.paths.entry((frame.node, gw)).or_default().record(rtt);
        }
        wire::Message::ChannelRequest { channel } => {
            // Requests for unknown channel ids are dropped (chapter 03 §4.3).
            if let Some(sink) = audio.sinks.iter_mut().find(|s| s.id == channel) {
                sink.requesters
                    .insert(frame.node, now + frame.ttl as i64 * 1_000_000);
                eng.notify();
            }
        }
        wire::Message::StopChannelRequest { channel } => {
            // Immediate removal of the requester (chapter 03 §4.3).
            if let Some(sink) = audio.sinks.iter_mut().find(|s| s.id == channel) {
                sink.requesters.remove(&frame.node);
            }
        }
        wire::Message::ChannelByes { channels } => {
            // Remove the (channel, gateway) entries named by their publisher
            // (chapter 03 §4.4, §7.3).
            audio.known.retain(|(id, g), kc| {
                !(*g == gw && kc.peer == frame.node && channels.contains(id))
            });
        }
        wire::Message::AudioBuffer(buffer) => {
            // Cross-session buffers have no defined beat mapping (ch. 03 §6.4).
            if buffer.session != session {
                return;
            }
            // Buffers for channels with no local source are discarded after
            // parsing (chapter 03 §7.4).
            if let Some(source) = audio.sources.get_mut(&buffer.channel) {
                deliver(source, &buffer, &timeline, now);
            }
        }
    }
}

fn deliver(source: &mut Source, buf: &wire::AudioBuffer, timeline: &Timeline, now: i64) {
    let delta = math::session_offset(timeline, source.quantum);
    let samples = buf.samples();
    let ch = buf.num_channels.max(1) as usize;
    let mut at = 0usize;
    // Each chunk is delivered as a separate unit (chapter 03 §7.4).
    for chunk in &buf.chunks {
        // Loss from sequence gaps (chapter 03 §5.3): the data plane is
        // open-loop and nothing reports loss to the sender, so the receiver
        // SHOULD surface it to the application (chapter 03 §5.8).
        match source.next_seq_expected {
            None => source.next_seq_expected = Some(chunk.seq + 1),
            Some(expected) if chunk.seq >= expected => {
                source.lost_chunks += chunk.seq - expected;
                source.next_seq_expected = Some(chunk.seq + 1);
            }
            // A reordered chunk arriving after its gap was counted.
            Some(_) => source.lost_chunks = source.lost_chunks.saturating_sub(1),
        }
        let end = (at + chunk.num_frames as usize * ch).min(samples.len());
        source.inbox.push_back(ReceivedChunk {
            samples: samples[at..end].to_vec(),
            begin_app_beat: (chunk.begin_beats + delta) as f64 / 1e6,
            tempo_micros_per_beat: chunk.tempo,
            sample_rate: buf.sample_rate,
            channels: buf.num_channels,
            seq: chunk.seq,
        });
        at = end;
    }
    while source.inbox.len() > INBOX_LIMIT {
        source.inbox.pop_front();
    }
    source.last_buffer_at = now;
}

// -------------------------------------------------------------- outbound

fn encode_announcement(
    node: NodeId,
    session: SessionId,
    peer_name: &[u8],
    channels: &[wire::ChannelInfo],
    ping: Option<i64>,
) -> Vec<u8> {
    wire::encode(&wire::Frame::new(
        node,
        wire::Message::PeerAnnouncement {
            session,
            peer_name: peer_name.to_vec(),
            channels: channels.to_vec(),
            ping,
        },
    ))
}

/// One announcement round: the full channel list split across messages
/// within the payload budget; only the round's first message carries the
/// ping (chapter 03 §4.1).
fn build_announcement_round(
    node: NodeId,
    session: SessionId,
    audio: &AudioState,
    now: i64,
) -> Vec<Vec<u8>> {
    let mut remaining: VecDeque<wire::ChannelInfo> = audio
        .sinks
        .iter()
        .map(|s| wire::ChannelInfo {
            name: s.name.clone(),
            id: s.id,
        })
        .collect();
    let mut round = Vec::new();
    loop {
        let ping = round.is_empty().then_some(now);
        let mut taken: Vec<wire::ChannelInfo> = Vec::new();
        while let Some(ch) = remaining.pop_front() {
            taken.push(ch);
            let size = encode_announcement(node, session, &audio.peer_name, &taken, ping).len();
            if size > 20 + wire::MAX_PAYLOAD && taken.len() > 1 {
                remaining.push_front(taken.pop().unwrap());
                break;
            }
        }
        round.push(encode_announcement(
            node,
            session,
            &audio.peer_name,
            &taken,
            ping,
        ));
        if remaining.is_empty() {
            return round;
        }
    }
}

/// Send a Channel(Stop)Request to a channel's publisher over the best path
/// (chapter 03 §4.3).
fn send_request(
    node: NodeId,
    peers: &Peers,
    gateways: &[Arc<Gateway>],
    audio: &AudioState,
    channel: ChannelId,
    stop: bool,
) {
    let Some(publisher) = audio
        .known
        .iter()
        .find(|((id, _), _)| *id == channel)
        .map(|(_, kc)| kc.peer)
    else {
        return;
    };
    let Some((gw, ep)) = best_path(peers, &audio.paths, publisher) else {
        return;
    };
    let message = if stop {
        wire::Message::StopChannelRequest { channel }
    } else {
        wire::Message::ChannelRequest { channel }
    };
    let frame = wire::Frame::new(node, message);
    send_via(gateways, gw, &wire::encode(&frame), ep);
}

/// Send ChannelByes for the given ids to every session-peer audio endpoint,
/// split within the payload budget (chapter 03 §4.4).
fn send_byes(
    node: NodeId,
    session: SessionId,
    peers: &Peers,
    gateways: &[Arc<Gateway>],
    channels: &[ChannelId],
) {
    if channels.is_empty() {
        return;
    }
    let per_message = (wire::MAX_PAYLOAD - 12) / 8; // entry header + count
    for ids in channels.chunks(per_message) {
        let frame = wire::Frame::new(
            node,
            wire::Message::ChannelByes {
                channels: ids.to_vec(),
            },
        );
        let bytes = wire::encode(&frame);
        for (gw, ep) in session_endpoints(peers, session) {
            send_via(gateways, gw, &bytes, ep);
        }
    }
}

/// Append beat-stamped material to a sink and transmit every full datagram
/// (chapter 03 §5.3 chunking, §5.6 sizing, §5.7 transmission conditions).
#[allow(clippy::too_many_arguments)]
pub fn write_sink(
    eng: &Engine,
    st: &mut State,
    channel: ChannelId,
    samples: &[i16],
    sample_rate: u32,
    num_channels: u8,
    begin_app_beat: f64,
    quantum: i64,
) {
    let now = eng.now();
    let timeline = st.timeline;
    let session = st.session;
    let node = st.node;
    let State {
        audio,
        peers,
        gateways,
        ..
    } = st;
    let Some(audio) = audio.as_mut() else { return };
    let Some(sink) = audio.sinks.iter_mut().find(|s| s.id == channel) else {
        return;
    };

    // Sender side of beat alignment (chapter 03 §6.3): wire beats are the
    // app beat minus the session offset for the sink's quantum.
    let tempo = timeline.tempo;
    let delta = math::session_offset(&timeline, quantum);
    let begin_wire = (begin_app_beat * 1e6).round() as i64 - delta;

    // A format change flushes pending material (chapter 03 §5.3); the
    // un-filled remainder is dropped, indistinguishable from datagram loss.
    if sink.format != Some((sample_rate, num_channels)) {
        sink.segments.clear();
        sink.format = Some((sample_rate, num_channels));
    }
    // Contiguity: extend the current run when the tempo matches and the new
    // material starts exactly at its end (chapter 03 §5.3).
    let ch = num_channels.max(1) as usize;
    let contiguous = sink.segments.back().is_some_and(|seg| {
        let frames = (seg.samples.len() / ch) as i64;
        seg.tempo == tempo
            && (seg.begin + frames_to_micro_beats(frames, sample_rate, tempo) - begin_wire).abs()
                <= 1
    });
    if contiguous {
        sink.segments.back_mut().unwrap().samples.extend(samples);
    } else {
        sink.segments.push_back(Segment {
            begin: begin_wire,
            tempo,
            samples: samples.to_vec(),
        });
    }

    // A sink with no unexpired requester is silent (chapter 03 §5.7);
    // discard instead of accumulating.
    sink.requesters.retain(|_, expiry| *expiry > now);
    if sink.requesters.is_empty() {
        sink.segments.clear();
        return;
    }

    // Drain full datagrams (chapter 03 §5.6, §3.1).
    let fill = eng.config.fill_audio_datagrams;
    let datagrams: Vec<Vec<u8>> =
        drain_datagrams(sink, channel, session, sample_rate, num_channels, fill)
            .into_iter()
            .map(|buffer| wire::encode(&wire::Frame::new(node, wire::Message::AudioBuffer(buffer))))
            .collect();

    // One unicast copy per unexpired requester, over its best path
    // (chapter 03 §5.7).
    let requesters: Vec<NodeId> = sink.requesters.keys().copied().collect();
    for peer in requesters {
        if let Some((gw, ep)) = best_path(peers, &audio.paths, peer) {
            for bytes in &datagrams {
                send_via(gateways, gw, bytes, ep);
            }
        }
    }
}

/// Pack full datagrams from a sink's pending segments and consume the
/// packed material; a partial datagram's worth stays pending.
///
/// Default sizing matches the reference sender: a flat 502-byte sample
/// budget per datagram, chunk records uncounted (chapter 03 §5.6). Fill
/// mode packs each datagram toward the 1176-byte SHOULD payload budget
/// (chapter 03 §3.1) instead, counting real chunk-record overhead and
/// keeping every chunk within the 512-frame bound that v1 endpoints impose
/// (chapter 03 §5.9 [N]) — the efficient corner of v1 the reference sender
/// leaves unused (≈ 2.2× fewer datagrams for the same audio).
fn drain_datagrams(
    sink: &mut Sink,
    channel: ChannelId,
    session: SessionId,
    sample_rate: u32,
    num_channels: u8,
    fill: bool,
) -> Vec<wire::AudioBuffer> {
    let ch = num_channels.max(1) as usize;
    let frame_bytes = 2 * ch;
    // The byte region the planner spends: sample bytes alone (reference
    // sizing) or chunk records + sample bytes (fill sizing).
    let (budget, record_cost, chunk_cap) = if fill {
        (
            wire::MAX_PAYLOAD - wire::AUDIO_FIXED_OVERHEAD,
            wire::CHUNK_RECORD_SIZE,
            wire::V1_MAX_CHUNK_FRAMES,
        )
    } else {
        (wire::SAMPLE_BYTE_CAP, 0, usize::MAX)
    };

    let mut out = Vec::new();
    loop {
        // Plan one datagram: frames per chunk, drawn in order from the
        // leading segments; a segment longer than the chunk cap splits.
        let mut takes: Vec<usize> = Vec::new();
        let mut left = budget;
        let mut exhausted = false;
        'plan: for seg in &sink.segments {
            let mut seg_frames = seg.samples.len() / ch;
            while seg_frames > 0 {
                if left < record_cost + frame_bytes {
                    exhausted = true;
                    break 'plan;
                }
                let take = seg_frames
                    .min(chunk_cap)
                    .min((left - record_cost) / frame_bytes);
                takes.push(take);
                left -= record_cost + take * frame_bytes;
                seg_frames -= take;
            }
        }
        // Emit only full datagrams; a partial one waits for more material.
        if takes.is_empty() || (!exhausted && left >= record_cost + frame_bytes) {
            return out;
        }

        // Consume the plan.
        let mut chunks = Vec::new();
        let mut data: Vec<i16> = Vec::new();
        for take in takes {
            // The planner skips sub-frame residue (a segment shorter than
            // one frame); drop it here so the two stay aligned.
            while sink.segments.front().is_some_and(|s| s.samples.len() < ch) {
                sink.segments.pop_front();
            }
            let seg = sink.segments.front_mut().expect("planned frames");
            chunks.push(wire::Chunk {
                seq: sink.next_seq,
                num_frames: take as u16,
                begin_beats: seg.begin,
                tempo: seg.tempo,
            });
            sink.next_seq += 1;
            data.extend(seg.samples.drain(..take * ch));
            seg.begin += frames_to_micro_beats(take as i64, sample_rate, seg.tempo);
            if seg.samples.is_empty() {
                sink.segments.pop_front();
            }
        }
        out.push(wire::AudioBuffer {
            channel,
            session,
            chunks,
            codec: wire::CODEC_PCM_I16,
            sample_rate,
            num_channels,
            sample_data: wire::AudioBuffer::encode_samples(&data),
        });
    }
}

// ---------------------------------------------------------- housekeeping

pub fn peer_left(_eng: &Engine, st: &mut State, node: NodeId) {
    if let Some(audio) = st.audio.as_mut() {
        // Channels of a departed publisher disappear (chapter 03 §7.3).
        audio.known.retain(|_, kc| kc.peer != node);
        audio.paths.retain(|(n, _), _| *n != node);
    }
}

pub fn housekeeping(eng: &Engine, st: &mut State, now: i64) -> i64 {
    let session = st.session;
    let node = st.node;
    let State {
        audio,
        peers,
        gateways,
        ..
    } = st;
    let Some(audio) = audio.as_mut() else {
        return i64::MAX;
    };
    let mut next = audio.last_announce_at + ANNOUNCE_PERIOD;

    // Announcement round (chapter 03 §4.1): unicast to the audio endpoint
    // of every known session peer.
    if now >= audio.last_announce_at + ANNOUNCE_PERIOD {
        let round = build_announcement_round(node, session, audio, now);
        for (gw, ep) in session_endpoints(peers, session) {
            for msg in &round {
                send_via(gateways, gw, msg, ep);
            }
        }
        audio.last_announce_at = now;
        next = now + ANNOUNCE_PERIOD;
    }

    // Source keepalive: re-send ChannelRequest every 5 s (chapter 03 §4.3).
    let mut due: Vec<ChannelId> = Vec::new();
    for (id, source) in audio.sources.iter_mut() {
        if now >= source.last_request_at + REQUEST_PERIOD {
            source.last_request_at = now;
            due.push(*id);
        }
        next = next.min(source.last_request_at + REQUEST_PERIOD);
    }
    for id in due {
        send_request(node, peers, gateways, audio, id, false);
    }

    // Requester expiry (chapter 03 §7.2), with 1 s prune padding.
    for sink in audio.sinks.iter_mut() {
        if let Some(earliest) = sink.requesters.values().copied().min() {
            if now >= earliest + PRUNE_PADDING {
                sink.requesters.retain(|_, e| *e > now);
            }
            if let Some(e) = sink.requesters.values().copied().min() {
                next = next.min(e + PRUNE_PADDING);
            }
        }
    }
    // Channel timeout (chapter 03 §7.3), same padding.
    if let Some(earliest) = audio.known.values().map(|k| k.deadline).min() {
        if now >= earliest + PRUNE_PADDING {
            audio.known.retain(|_, k| k.deadline > now);
        }
        if let Some(e) = audio.known.values().map(|k| k.deadline).min() {
            next = next.min(e + PRUNE_PADDING);
        }
    }

    let _ = eng;
    next
}

/// Withdraw all published channels before the messenger goes away
/// (chapter 03 §4.4).
pub fn shutdown(_eng: &Engine, st: &mut State) {
    let session = st.session;
    let node = st.node;
    let State {
        audio,
        peers,
        gateways,
        ..
    } = st;
    let Some(audio) = audio.as_ref() else { return };
    let ids: Vec<ChannelId> = audio.sinks.iter().map(|s| s.id).collect();
    send_byes(node, session, peers, gateways, &ids);
    st.audio = None;
}

// ------------------------------------------------------------ public ops

pub fn enable(eng: &Engine, st: &mut State, peer_name: &str) {
    if st.audio.is_none() {
        st.audio = Some(AudioState::new(peer_name));
        // The aep4 advertisement switches on with the next gossip (ch. 03 §2).
        eng.schedule_broadcast(st);
        eng.notify();
    }
}

pub fn disable(eng: &Engine, st: &mut State) {
    if st.audio.is_some() {
        shutdown(eng, st);
        // Peer state without an audio endpoint clears it remotely (ch. 03 §2).
        eng.schedule_broadcast(st);
    }
}

pub fn publish(eng: &Engine, st: &mut State, name: &str) -> Option<ChannelId> {
    let audio = st.audio.as_mut()?;
    let id = random_id();
    let mut name = name.as_bytes().to_vec();
    name.truncate(wire::MAX_NAME);
    audio.sinks.push(Sink {
        id,
        name,
        requesters: HashMap::new(),
        next_seq: 1,
        segments: VecDeque::new(),
        format: None,
    });
    // Announce promptly rather than waiting out the period.
    audio.last_announce_at = 0;
    eng.notify();
    Some(id)
}

pub fn unpublish(_eng: &Engine, st: &mut State, channel: ChannelId) {
    let session = st.session;
    let node = st.node;
    let State {
        audio,
        peers,
        gateways,
        ..
    } = st;
    let Some(audio) = audio.as_mut() else { return };
    let before = audio.sinks.len();
    audio.sinks.retain(|s| s.id != channel);
    if audio.sinks.len() != before {
        send_byes(node, session, peers, gateways, &[channel]);
    }
}

pub fn subscribe(eng: &Engine, st: &mut State, channel: ChannelId, quantum: i64) {
    let now = eng.now();
    let node = st.node;
    let State {
        audio,
        peers,
        gateways,
        ..
    } = st;
    let Some(audio) = audio.as_mut() else { return };
    audio.sources.entry(channel).or_insert(Source {
        quantum,
        last_request_at: now,
        last_buffer_at: i64::MIN / 2,
        inbox: VecDeque::new(),
        next_seq_expected: None,
        lost_chunks: 0,
    });
    send_request(node, peers, gateways, audio, channel, false);
    eng.notify();
}

/// True while subscribed audio arrived within the receiving window.
pub fn is_receiving(st: &State, channel: ChannelId, now: i64) -> bool {
    st.audio
        .as_ref()
        .and_then(|a| a.sources.get(&channel))
        .is_some_and(|s| now - s.last_buffer_at <= RECEIVING_WINDOW)
}

pub fn unsubscribe(_eng: &Engine, st: &mut State, channel: ChannelId) {
    let node = st.node;
    let State {
        audio,
        peers,
        gateways,
        ..
    } = st;
    let Some(audio) = audio.as_mut() else { return };
    if audio.sources.remove(&channel).is_some() {
        send_request(node, peers, gateways, audio, channel, true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tactus_wire::types::Id;

    /// §5.9 [N]: a receiver MUST bound its decode-to-render copy by its own
    /// capacity rather than overrun. This receiver stages into the decoded
    /// vector itself (no fixed scratch buffer), so a single chunk above the
    /// reference's 512-frame endpoint limit is delivered intact.
    #[test]
    fn deliver_stages_chunk_above_512_frames_intact() {
        let samples: Vec<i16> = (0..550).map(|i| (i * 7 - 500) as i16).collect();
        let buf = wire::AudioBuffer {
            channel: Id(*b"CHANNEL1"),
            session: Id(*b"SESSIONX"),
            chunks: vec![wire::Chunk {
                seq: 3,
                num_frames: 550,
                begin_beats: 4_000_000,
                tempo: 500_000,
            }],
            codec: wire::CODEC_PCM_I16,
            sample_rate: 48_000,
            num_channels: 1,
            sample_data: wire::AudioBuffer::encode_samples(&samples),
        };
        let timeline = Timeline {
            tempo: 500_000,
            beat_origin: 0,
            time_origin: 0,
        };
        let mut source = test_source();
        deliver(&mut source, &buf, &timeline, 123);
        assert_eq!(source.inbox.len(), 1);
        let chunk = &source.inbox[0];
        assert_eq!(chunk.samples, samples);
        assert_eq!(chunk.seq, 3);
        assert_eq!(source.last_buffer_at, 123);
    }

    fn test_source() -> Source {
        Source {
            quantum: 4_000_000,
            last_request_at: 0,
            last_buffer_at: i64::MIN / 2,
            inbox: VecDeque::new(),
            next_seq_expected: None,
            lost_chunks: 0,
        }
    }

    fn seq_buffer(seqs: &[u64]) -> wire::AudioBuffer {
        let frames = 4usize;
        wire::AudioBuffer {
            channel: Id(*b"CHANNEL1"),
            session: Id(*b"SESSIONX"),
            chunks: seqs
                .iter()
                .map(|&seq| wire::Chunk {
                    seq,
                    num_frames: frames as u16,
                    begin_beats: 0,
                    tempo: 500_000,
                })
                .collect(),
            codec: wire::CODEC_PCM_I16,
            sample_rate: 48_000,
            num_channels: 1,
            sample_data: wire::AudioBuffer::encode_samples(&vec![0; frames * seqs.len()]),
        }
    }

    fn test_sink(frames: usize, ch: usize) -> Sink {
        Sink {
            id: Id(*b"CHANNEL1"),
            name: b"sink".to_vec(),
            requesters: HashMap::new(),
            next_seq: 1,
            segments: VecDeque::from([Segment {
                begin: 0,
                tempo: 500_000,
                samples: vec![0i16; frames * ch],
            }]),
            format: Some((48_000, ch as u8)),
        }
    }

    fn drain(sink: &mut Sink, ch: u8, fill: bool) -> Vec<wire::AudioBuffer> {
        drain_datagrams(sink, Id(*b"CHANNEL1"), Id(*b"SESSIONX"), 48_000, ch, fill)
    }

    fn encoded_len(buf: &wire::AudioBuffer) -> usize {
        wire::encode(&wire::Frame::new(
            Id(*b"NODEID01"),
            wire::Message::AudioBuffer(buf.clone()),
        ))
        .len()
    }

    /// Reference sizing (chapter 03 §5.6): 251-frame mono datagrams at the
    /// 502-byte sample cap (576 bytes on the wire [W]); the partial
    /// remainder stays pending.
    #[test]
    fn drain_reference_sizing_matches_the_reference_wire_shape() {
        let mut sink = test_sink(600, 1);
        let bufs = drain(&mut sink, 1, false);
        assert_eq!(bufs.len(), 2);
        for buf in &bufs {
            assert_eq!(buf.total_frames(), 251);
            assert_eq!(encoded_len(buf), 576);
        }
        let pending: usize = sink.segments.iter().map(|s| s.samples.len()).sum();
        assert_eq!(pending, 600 - 2 * 251);
    }

    /// Fill sizing: each datagram packs to the 1176-byte SHOULD payload
    /// budget (chapter 03 §3.1) with every chunk within the v1 512-frame
    /// receive bound (chapter 03 §5.9 [N]) — 548 mono frames per datagram
    /// as a 512 + 36 chunk pair, 2.2× the reference's 251.
    #[test]
    fn drain_fill_sizing_packs_the_datagram_within_v1_bounds() {
        let mut sink = test_sink(1200, 1);
        let bufs = drain(&mut sink, 1, true);
        assert_eq!(bufs.len(), 2);
        let mut expected_seq = 1;
        for buf in &bufs {
            assert_eq!(buf.total_frames(), 548);
            assert_eq!(encoded_len(buf), 20 + wire::MAX_PAYLOAD);
            for c in &buf.chunks {
                assert!((c.num_frames as usize) <= wire::V1_MAX_CHUNK_FRAMES);
                assert_eq!(c.seq, expected_seq);
                expected_seq += 1;
            }
        }
        let pending: usize = sink.segments.iter().map(|s| s.samples.len()).sum();
        assert_eq!(pending, 1200 - 2 * 548);
    }

    /// §5.8: the receiver surfaces loss from sequence gaps; the first seen
    /// chunk seeds the counter (mid-stream subscription is not loss), and a
    /// reordered late arrival takes its gap back off the count.
    #[test]
    fn sequence_gaps_are_surfaced_as_loss() {
        let timeline = Timeline {
            tempo: 500_000,
            beat_origin: 0,
            time_origin: 0,
        };
        let mut source = test_source();
        deliver(&mut source, &seq_buffer(&[5]), &timeline, 0);
        assert_eq!(source.lost_chunks, 0, "mid-stream start is not loss");
        deliver(&mut source, &seq_buffer(&[6, 7]), &timeline, 1);
        assert_eq!(source.lost_chunks, 0);
        deliver(&mut source, &seq_buffer(&[10]), &timeline, 2);
        assert_eq!(source.lost_chunks, 2, "chunks 8 and 9 missing");
        deliver(&mut source, &seq_buffer(&[8]), &timeline, 3);
        assert_eq!(source.lost_chunks, 1, "8 arrived late, only 9 missing");
        assert_eq!(source.inbox.len(), 5);
    }
}
