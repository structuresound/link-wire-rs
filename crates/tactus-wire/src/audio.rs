//! LinkAudio v1 messages (spec chapter 03).

use crate::codec::{Entries, Error, Reader, Result, Writer};
use crate::types::{ChannelId, NodeId, SessionId};

/// Frame magic: ASCII `chnnlsv` + version byte 0x01 (chapter 03 §3).
pub const MAGIC: [u8; 8] = *b"chnnlsv\x01";
/// Maximum datagram size, chosen to avoid IP fragmentation (chapter 03 §3.1).
pub const MAX_MESSAGE_SIZE: usize = 1200;
/// Sender-side payload budget (chapter 03 §3.1).
pub const MAX_PAYLOAD: usize = 1176;
/// Receivers accept payloads up to this (chapter 03 §3.1, resolved).
pub const MAX_RECV_PAYLOAD: usize = 1180;
/// Sender-side cap on sample bytes per datagram (chapter 03 §5.6).
pub const SAMPLE_BYTE_CAP: usize = 502;
/// Peer/channel names are truncated to this before transmit (chapter 03 §8).
pub const MAX_NAME: usize = 256;
/// Announcement / request validity, seconds (chapter 03 §10).
pub const TTL_SECONDS: u8 = 5;
/// Announcement nominal period (chapter 03 §4.1).
pub const ANNOUNCE_PERIOD_MS: u64 = 250;
/// Source re-request period (chapter 03 §4.3).
pub const REQUEST_PERIOD_MS: u64 = 5000;
/// PCM i16 codec value (chapter 03 §5.4).
pub const CODEC_PCM_I16: u8 = 1;
/// Wire size of one chunk record (chapter 03 §5.3).
pub const CHUNK_RECORD_SIZE: usize = 26;
/// Fixed non-chunk bytes of an AudioBuffer payload: channel + session +
/// chunk count + codec + rate + channels + numBytes (chapter 03 §5.6).
pub const AUDIO_FIXED_OVERHEAD: usize = 28;
/// Per-chunk frame bound a sender MUST respect toward v1 peers — the
/// reference endpoint stages each chunk in a fixed 512-sample buffer
/// (chapter 03 §5.9 [N]).
pub const V1_MAX_CHUNK_FRAMES: usize = 512;

const TYPE_PEER_ANNOUNCEMENT: u8 = 1;
const TYPE_CHANNEL_BYES: u8 = 2;
const TYPE_PONG: u8 = 3;
const TYPE_CHANNEL_REQUEST: u8 = 4;
const TYPE_STOP_CHANNEL_REQUEST: u8 = 5;
const TYPE_AUDIO_BUFFER: u8 = 6;

mod keys {
    use crate::codec::fourcc;

    pub const SESS: u32 = fourcc(b"sess");
    pub const PI: u32 = fourcc(b"__pi"); // peer info (display name)
    pub const AUCA: u32 = fourcc(b"auca"); // channel announcements
    pub const AUCB: u32 = fourcc(b"aucb"); // channel byes
    pub const CHID: u32 = fourcc(b"chid"); // channel request id
    pub const HT: u32 = fourcc(b"__ht"); // keepalive ping
}

/// One announced channel: display name + stable identifier (chapter 03 §4.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelInfo {
    pub name: Vec<u8>,
    pub id: ChannelId,
}

/// One frame-run-to-beat-grid mapping (chapter 03 §5.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chunk {
    /// Increases by 1 per chunk the sender creates on this channel; first = 1.
    pub seq: u64,
    /// Frames covered by this chunk.
    pub num_frames: u16,
    /// Session beat time of the chunk's first frame, micro-beats (§6).
    pub begin_beats: i64,
    /// Tempo during this chunk, µs per beat.
    pub tempo: i64,
}

/// The bare audio-buffer structure (chapter 03 §5): not wrapped in a payload
/// container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioBuffer {
    pub channel: ChannelId,
    /// The sender's Link session; the beat mapping is undefined across
    /// sessions (§6.4).
    pub session: SessionId,
    pub chunks: Vec<Chunk>,
    /// Codec value; only [`CODEC_PCM_I16`] is defined (§5.4).
    pub codec: u8,
    pub sample_rate: u32,
    /// Interleaved channel count (1 or 2 in the public reference API).
    pub num_channels: u8,
    /// Encoded sample bytes, exactly as on the wire (big-endian i16 for
    /// codec 1, frame-interleaved; §5.5).
    pub sample_data: Vec<u8>,
}

impl AudioBuffer {
    /// Total frames covered by the chunk list.
    pub fn total_frames(&self) -> u32 {
        self.chunks.iter().map(|c| c.num_frames as u32).sum()
    }

    /// Decode codec-1 sample data to native-endian samples.
    pub fn samples(&self) -> Vec<i16> {
        self.sample_data
            .chunks_exact(2)
            .map(|b| i16::from_be_bytes([b[0], b[1]]))
            .collect()
    }

    /// Encode native-endian samples as codec-1 wire bytes.
    pub fn encode_samples(samples: &[i16]) -> Vec<u8> {
        let mut out = Vec::with_capacity(samples.len() * 2);
        for s in samples {
            out.extend_from_slice(&s.to_be_bytes());
        }
        out
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    /// Unicast peer/channel announcement with optional embedded ping
    /// (chapter 03 §4.1).
    PeerAnnouncement {
        session: SessionId,
        peer_name: Vec<u8>,
        channels: Vec<ChannelInfo>,
        /// `__ht` keepalive ping: sender's local clock µs. Present on
        /// exactly the first announcement of a per-destination round.
        ping: Option<i64>,
    },
    /// Withdraws the listed channels published by the header's sender
    /// (chapter 03 §4.4).
    ChannelByes { channels: Vec<ChannelId> },
    /// Echoes a ping's `__ht` unchanged (chapter 03 §4.2).
    Pong { host_time: i64 },
    /// Subscribe to a channel for header-`ttl` seconds (chapter 03 §4.3).
    ChannelRequest { channel: ChannelId },
    /// Immediately withdraw the sender's request (chapter 03 §4.3); ttl 0.
    StopChannelRequest { channel: ChannelId },
    /// Beat-stamped audio (chapter 03 §5); ttl 0.
    AudioBuffer(AudioBuffer),
}

/// One LinkAudio datagram (chapter 03 §3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub ttl: u8,
    /// Always 0 in this version; receivers ignore frames with other values.
    pub group_id: u16,
    /// Sender's NodeId.
    pub node: NodeId,
    pub message: Message,
}

impl Frame {
    /// A frame with the ttl the protocol nominally sends for this message
    /// type (chapter 03 §3.2) and groupId 0.
    pub fn new(node: NodeId, message: Message) -> Frame {
        let ttl = match message {
            Message::PeerAnnouncement { .. }
            | Message::ChannelByes { .. }
            | Message::Pong { .. }
            | Message::ChannelRequest { .. } => TTL_SECONDS,
            Message::StopChannelRequest { .. } | Message::AudioBuffer(_) => 0,
        };
        Frame {
            ttl,
            group_id: 0,
            node,
            message,
        }
    }
}

pub fn encode(frame: &Frame) -> Vec<u8> {
    let mut w = Writer::new();
    w.bytes(&MAGIC);
    w.u8(match &frame.message {
        Message::PeerAnnouncement { .. } => TYPE_PEER_ANNOUNCEMENT,
        Message::ChannelByes { .. } => TYPE_CHANNEL_BYES,
        Message::Pong { .. } => TYPE_PONG,
        Message::ChannelRequest { .. } => TYPE_CHANNEL_REQUEST,
        Message::StopChannelRequest { .. } => TYPE_STOP_CHANNEL_REQUEST,
        Message::AudioBuffer(_) => TYPE_AUDIO_BUFFER,
    });
    w.u8(frame.ttl);
    w.u16(frame.group_id);
    frame.node.write(&mut w);
    match &frame.message {
        Message::PeerAnnouncement {
            session,
            peer_name,
            channels,
            ping,
        } => {
            // Entry order as the reference emits (manifest: sess, __pi,
            // auca, __ht); receivers must not rely on it.
            w.entry(keys::SESS, |w| session.write(w));
            w.entry(keys::PI, |w| w.string(peer_name));
            w.entry(keys::AUCA, |w| {
                w.u32(channels.len() as u32);
                for ch in channels {
                    w.string(&ch.name);
                    ch.id.write(w);
                }
            });
            if let Some(ht) = ping {
                w.entry(keys::HT, |w| w.i64(*ht));
            }
        }
        Message::ChannelByes { channels } => {
            w.entry(keys::AUCB, |w| {
                w.u32(channels.len() as u32);
                for id in channels {
                    id.write(w);
                }
            });
        }
        Message::Pong { host_time } => {
            w.entry(keys::HT, |w| w.i64(*host_time));
        }
        Message::ChannelRequest { channel } | Message::StopChannelRequest { channel } => {
            w.entry(keys::CHID, |w| channel.write(w));
        }
        Message::AudioBuffer(buf) => {
            // Bare structure, no payload container (§5.1, resolved [W]).
            buf.channel.write(&mut w);
            buf.session.write(&mut w);
            w.u32(buf.chunks.len() as u32);
            for c in &buf.chunks {
                w.u64(c.seq);
                w.u16(c.num_frames);
                w.i64(c.begin_beats);
                w.i64(c.tempo);
            }
            w.u8(buf.codec);
            w.u32(buf.sample_rate);
            w.u8(buf.num_channels);
            w.u16(buf.sample_data.len() as u16);
            w.bytes(&buf.sample_data);
        }
    }
    debug_assert!(w.len() <= MAX_MESSAGE_SIZE);
    w.into_vec()
}

pub fn decode(datagram: &[u8]) -> Result<Frame> {
    let mut r = Reader::new(datagram);
    if r.remaining() < 20 {
        return Err(Error::Truncated);
    }
    if r.take(8)? != MAGIC {
        return Err(Error::BadMagic);
    }
    let msg_type = r.u8()?;
    let ttl = r.u8()?;
    let group_id = r.u16()?;
    let node = NodeId::read(&mut r)?;
    let payload = r.rest();
    let message = match msg_type {
        TYPE_PEER_ANNOUNCEMENT => decode_announcement(payload)?,
        TYPE_CHANNEL_BYES => decode_byes(payload)?,
        TYPE_PONG => {
            let mut host_time = None;
            for entry in Entries::new(payload) {
                let entry = entry?;
                if entry.key == keys::HT {
                    host_time = Some(entry.decode(|r| r.i64())?);
                }
            }
            Message::Pong {
                host_time: host_time.ok_or(Error::Malformed("pong missing __ht"))?,
            }
        }
        TYPE_CHANNEL_REQUEST | TYPE_STOP_CHANNEL_REQUEST => {
            let mut channel = None;
            for entry in Entries::new(payload) {
                let entry = entry?;
                if entry.key == keys::CHID {
                    channel = Some(entry.decode(ChannelId::read)?);
                }
            }
            let channel = channel.ok_or(Error::Malformed("request missing chid"))?;
            if msg_type == TYPE_CHANNEL_REQUEST {
                Message::ChannelRequest { channel }
            } else {
                Message::StopChannelRequest { channel }
            }
        }
        TYPE_AUDIO_BUFFER => Message::AudioBuffer(decode_audio_buffer(payload)?),
        other => return Err(Error::UnknownMessageType(other)),
    };
    Ok(Frame {
        ttl,
        group_id,
        node,
        message,
    })
}

fn decode_announcement(payload: &[u8]) -> Result<Message> {
    let mut session = None;
    let mut peer_name = Vec::new();
    let mut channels = Vec::new();
    let mut ping = None;
    for entry in Entries::new(payload) {
        let entry = entry?;
        match entry.key {
            keys::SESS => session = Some(entry.decode(SessionId::read)?),
            keys::PI => peer_name = entry.decode(|r| Ok(r.string()?.to_vec()))?,
            keys::AUCA => {
                channels = entry.decode(|r| {
                    let count = r.u32()? as usize;
                    let mut out = Vec::new();
                    // A vector stops at its count or when the bytes run
                    // out, whichever comes first (chapter 00 §4.4).
                    for _ in 0..count {
                        if r.is_empty() {
                            break;
                        }
                        out.push(ChannelInfo {
                            name: r.string()?.to_vec(),
                            id: ChannelId::read(r)?,
                        });
                    }
                    Ok(out)
                })?
            }
            keys::HT => ping = Some(entry.decode(|r| r.i64())?),
            _ => {}
        }
    }
    Ok(Message::PeerAnnouncement {
        session: session.ok_or(Error::Malformed("announcement missing sess"))?,
        peer_name,
        channels,
        ping,
    })
}

fn decode_byes(payload: &[u8]) -> Result<Message> {
    let mut channels = Vec::new();
    for entry in Entries::new(payload) {
        let entry = entry?;
        if entry.key == keys::AUCB {
            channels = entry.decode(|r| {
                let count = r.u32()? as usize;
                let mut out = Vec::new();
                for _ in 0..count {
                    if r.is_empty() {
                        break;
                    }
                    out.push(ChannelId::read(r)?);
                }
                Ok(out)
            })?;
        }
    }
    Ok(Message::ChannelByes { channels })
}

fn decode_audio_buffer(payload: &[u8]) -> Result<AudioBuffer> {
    let mut r = Reader::new(payload);
    let channel = ChannelId::read(&mut r)?;
    let session = SessionId::read(&mut r)?;
    let chunk_count = r.u32()? as usize;
    if chunk_count == 0 {
        return Err(Error::Malformed("audio buffer with zero chunks"));
    }
    let mut chunks = Vec::with_capacity(chunk_count.min(64));
    for _ in 0..chunk_count {
        chunks.push(Chunk {
            seq: r.u64()?,
            num_frames: r.u16()?,
            begin_beats: r.i64()?,
            tempo: r.i64()?,
        });
    }
    let codec = r.u8()?;
    let sample_rate = r.u32()?;
    let num_channels = r.u8()?;
    let num_bytes = r.u16()? as usize;
    // The sample data must extend exactly to the end of the datagram (§5.2).
    if r.remaining() != num_bytes {
        return Err(Error::Malformed("sample bytes do not fill the datagram"));
    }
    let sample_data = r.take(num_bytes)?.to_vec();
    let buf = AudioBuffer {
        channel,
        session,
        chunks,
        codec,
        sample_rate,
        num_channels,
        sample_data,
    };
    // §5.4: codec 0 is rejected; unknown nonzero codecs SHOULD be rejected
    // rather than mis-decoded as PCM.
    if codec != CODEC_PCM_I16 {
        return Err(Error::Malformed("unsupported codec"));
    }
    if buf.total_frames() as usize * num_channels as usize * 2 != num_bytes {
        return Err(Error::Malformed("frame count and sample bytes disagree"));
    }
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Id;

    const NODE: Id = Id(*b"NODEID01");

    #[test]
    fn announcement_roundtrip() {
        let frame = Frame::new(
            NODE,
            Message::PeerAnnouncement {
                session: Id(*b"SESSIONX"),
                peer_name: b"Alice".to_vec(),
                channels: vec![ChannelInfo {
                    name: b"A Sink".to_vec(),
                    id: Id(*b"CHANNEL1"),
                }],
                ping: Some(123_456),
            },
        );
        let bytes = encode(&frame);
        // Manifest-observed announcement shape: ['sess','__pi','auca','__ht'];
        // a 5-byte peer name + one 6-byte channel name = 99 bytes [W].
        assert_eq!(bytes.len(), 99);
        assert_eq!(decode(&bytes).unwrap(), frame);
    }

    #[test]
    fn byes_pong_request_roundtrip() {
        for msg in [
            Message::ChannelByes {
                channels: vec![Id(*b"CHANNEL1"), Id(*b"CHANNEL2")],
            },
            Message::Pong { host_time: -99 },
            Message::ChannelRequest {
                channel: Id(*b"CHANNEL1"),
            },
            Message::StopChannelRequest {
                channel: Id(*b"CHANNEL1"),
            },
        ] {
            let frame = Frame::new(NODE, msg);
            assert_eq!(decode(&encode(&frame)).unwrap(), frame);
        }
    }

    #[test]
    fn request_ttls_follow_spec() {
        let req = Frame::new(
            NODE,
            Message::ChannelRequest {
                channel: Id(*b"CHANNEL1"),
            },
        );
        assert_eq!(req.ttl, 5);
        let stop = Frame::new(
            NODE,
            Message::StopChannelRequest {
                channel: Id(*b"CHANNEL1"),
            },
        );
        assert_eq!(stop.ttl, 0);
    }

    fn test_buffer() -> AudioBuffer {
        let samples: Vec<i16> = (0..251).map(|i| (i * 13 - 1000) as i16).collect();
        AudioBuffer {
            channel: Id(*b"CHANNEL1"),
            session: Id(*b"SESSIONX"),
            chunks: vec![Chunk {
                seq: 1,
                num_frames: 251,
                begin_beats: 4_000_000,
                tempo: 500_000,
            }],
            codec: CODEC_PCM_I16,
            sample_rate: 48_000,
            num_channels: 1,
            sample_data: AudioBuffer::encode_samples(&samples),
        }
    }

    #[test]
    fn audio_buffer_roundtrip_at_cap_is_576_bytes() {
        let frame = Frame::new(NODE, Message::AudioBuffer(test_buffer()));
        let bytes = encode(&frame);
        // §5.6: single-chunk datagram at the 502-byte cap = 576 bytes total.
        assert_eq!(bytes.len(), 576);
        assert_eq!(decode(&bytes).unwrap(), frame);
    }

    /// §5.9: the parse path has no 512-frame-per-chunk ceiling — it is
    /// bounded only by the datagram. The 512 limit is a reference *endpoint*
    /// behavior to interoperate against (never exceed it toward a v1 peer),
    /// not a decode rule to reproduce.
    #[test]
    fn audio_buffer_accepts_chunks_above_512_frames() {
        let samples: Vec<i16> = (0..550).map(|i| (i * 7 - 500) as i16).collect();
        let jumbo = AudioBuffer {
            chunks: vec![Chunk {
                seq: 9,
                num_frames: 550,
                begin_beats: 0,
                tempo: 500_000,
            }],
            sample_data: AudioBuffer::encode_samples(&samples),
            ..test_buffer()
        };
        let frame = Frame::new(NODE, Message::AudioBuffer(jumbo));
        let bytes = encode(&frame);
        assert_eq!(bytes.len(), 1174);
        assert_eq!(decode(&bytes).unwrap(), frame);

        // The probed-safe shape toward v1 peers: a full 1200-byte datagram
        // of two 275-frame chunks — 550 frames total, each chunk ≤ 512.
        let packed = AudioBuffer {
            chunks: (0..2)
                .map(|i| Chunk {
                    seq: 9 + i,
                    num_frames: 275,
                    begin_beats: i as i64 * 3_437_500,
                    tempo: 500_000,
                })
                .collect(),
            sample_data: AudioBuffer::encode_samples(&samples),
            ..test_buffer()
        };
        let frame = Frame::new(NODE, Message::AudioBuffer(packed));
        let bytes = encode(&frame);
        assert_eq!(bytes.len(), MAX_MESSAGE_SIZE);
        assert_eq!(decode(&bytes).unwrap(), frame);
    }

    #[test]
    fn audio_buffer_rejects_invalid() {
        let frame = Frame::new(NODE, Message::AudioBuffer(test_buffer()));
        let good = encode(&frame);

        // Zero chunks rejected (§9).
        let mut zero_chunks = good.clone();
        zero_chunks[36..40].copy_from_slice(&0u32.to_be_bytes());
        assert!(decode(&zero_chunks).is_err());

        // Codec 0 rejected (§5.4). Codec byte sits after the chunk record.
        let codec_at = 20 + 16 + 4 + CHUNK_RECORD_SIZE;
        let mut codec0 = good.clone();
        codec0[codec_at] = 0;
        assert!(decode(&codec0).is_err());

        // Truncated sample data rejected (§5.2).
        let mut short = good.clone();
        short.truncate(good.len() - 1);
        assert!(decode(&short).is_err());

        // Frame count / numBytes mismatch rejected (§5.4).
        let mut bad_frames = good.clone();
        let frames_at = 20 + 16 + 4 + 8;
        bad_frames[frames_at..frames_at + 2].copy_from_slice(&250u16.to_be_bytes());
        assert!(decode(&bad_frames).is_err());
    }

    #[test]
    fn sample_encoding_is_big_endian_interleaved() {
        let bytes = AudioBuffer::encode_samples(&[0x0102, -2]);
        assert_eq!(bytes, [0x01, 0x02, 0xff, 0xfe]);
    }
}
