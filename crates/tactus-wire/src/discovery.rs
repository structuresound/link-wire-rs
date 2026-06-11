//! Link peer discovery messages (spec chapter 01).

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};

use crate::codec::{Entries, Error, Reader, Result, Writer};
use crate::types::{
    decode_endpoint_value, write_endpoint_entry, NodeId, SessionId, StartStopState, Timeline,
};

/// Frame magic: ASCII `_asdp_v` + version byte 0x01 (chapter 01 §3).
pub const MAGIC: [u8; 8] = *b"_asdp_v\x01";
/// IPv4 multicast group (chapter 01 §2).
pub const MULTICAST_V4: Ipv4Addr = Ipv4Addr::new(224, 76, 78, 75);
/// IPv6 link-local multicast group (chapter 01 §2).
pub const MULTICAST_V6: Ipv6Addr = Ipv6Addr::new(0xff12, 0, 0, 0, 0, 0, 0, 0x8080);
/// UDP port of the multicast receive socket (chapter 01 §2).
pub const PORT: u16 = 20808;
/// Receivers accept up to 512 bytes; senders stay ≤ 511 (chapter 01 §3.1).
pub const MAX_MESSAGE_SIZE: usize = 512;
/// Header `ttl` the protocol nominally sends for state messages (chapter 01 §4).
pub const TTL_SECONDS: u8 = 5;
/// Nominal announcement period: ttl × 1000 / 20 ms (chapter 01 §4.1).
pub const ALIVE_PERIOD_MS: u64 = 250;
/// Minimum spacing between state-change broadcasts (chapter 01 §4.1).
pub const MIN_BROADCAST_SPACING_MS: u64 = 50;

const TYPE_ALIVE: u8 = 1;
const TYPE_RESPONSE: u8 = 2;
const TYPE_BYEBYE: u8 = 3;

mod keys {
    use crate::codec::fourcc;

    pub const TMLN: u32 = fourcc(b"tmln");
    pub const SESS: u32 = fourcc(b"sess");
    pub const STST: u32 = fourcc(b"stst");
    pub const MEP4: u32 = fourcc(b"mep4");
    pub const MEP6: u32 = fourcc(b"mep6");
    pub const AEP4: u32 = fourcc(b"aep4");
    pub const AEP6: u32 = fourcc(b"aep6");
}

/// The peer-state payload carried by Alive and Response (chapter 01 §6).
/// Every entry is optional on receive; a missing entry leaves the field
/// `None` (the corresponding state defaults to all-zero).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PeerState {
    pub timeline: Option<Timeline>,
    pub session: Option<SessionId>,
    pub start_stop: Option<StartStopState>,
    pub measurement_endpoint: Option<SocketAddr>,
    pub audio_endpoint: Option<SocketAddr>,
}

impl PeerState {
    /// Encode in the order the reference emits (chapter 01 §6); receivers
    /// must not rely on it.
    fn write(&self, w: &mut Writer) {
        if let Some(tl) = &self.timeline {
            w.entry(keys::TMLN, |w| tl.write(w));
        }
        if let Some(sess) = &self.session {
            w.entry(keys::SESS, |w| sess.write(w));
        }
        if let Some(stst) = &self.start_stop {
            w.entry(keys::STST, |w| stst.write(w));
        }
        write_endpoint_entry(w, keys::MEP4, keys::MEP6, &self.measurement_endpoint);
        write_endpoint_entry(w, keys::AEP4, keys::AEP6, &self.audio_endpoint);
    }

    fn read(payload: &[u8]) -> Result<PeerState> {
        let mut state = PeerState::default();
        for entry in Entries::new(payload) {
            let entry = entry?;
            match entry.key {
                keys::TMLN => state.timeline = Some(entry.decode(Timeline::read)?),
                keys::SESS => state.session = Some(entry.decode(SessionId::read)?),
                keys::STST => state.start_stop = Some(entry.decode(StartStopState::read)?),
                keys::MEP4 | keys::MEP6 => {
                    state.measurement_endpoint = Some(decode_endpoint_value(entry.value)?)
                }
                keys::AEP4 | keys::AEP6 => {
                    state.audio_endpoint = Some(decode_endpoint_value(entry.value)?)
                }
                _ => {} // skip unknown entries (chapter 00 §4.5 rule 2)
            }
        }
        Ok(state)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    /// Periodic multicast state dump (chapter 01 §4.1).
    Alive(PeerState),
    /// Unicast state dump answering an Alive (chapter 01 §4.2). Identical
    /// encoding to Alive.
    Response(PeerState),
    /// Departure announcement; empty payload, ttl 0 (chapter 01 §4.3).
    ByeBye,
}

/// One discovery datagram (chapter 01 §3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// Seconds the carried state stays valid (0 for ByeBye).
    pub ttl: u8,
    /// Session group; always 0 in this protocol version. Receivers must
    /// ignore frames with any other value — the codec surfaces it so the
    /// peer can apply that admission rule.
    pub group_id: u16,
    /// Sender's NodeId.
    pub node: NodeId,
    pub message: Message,
}

impl Frame {
    pub fn alive(node: NodeId, state: PeerState) -> Frame {
        Frame {
            ttl: TTL_SECONDS,
            group_id: 0,
            node,
            message: Message::Alive(state),
        }
    }

    pub fn response(node: NodeId, state: PeerState) -> Frame {
        Frame {
            ttl: TTL_SECONDS,
            group_id: 0,
            node,
            message: Message::Response(state),
        }
    }

    pub fn bye_bye(node: NodeId) -> Frame {
        Frame {
            ttl: 0,
            group_id: 0,
            node,
            message: Message::ByeBye,
        }
    }
}

pub fn encode(frame: &Frame) -> Vec<u8> {
    let mut w = Writer::new();
    w.bytes(&MAGIC);
    let (msg_type, state) = match &frame.message {
        Message::Alive(s) => (TYPE_ALIVE, Some(s)),
        Message::Response(s) => (TYPE_RESPONSE, Some(s)),
        Message::ByeBye => (TYPE_BYEBYE, None),
    };
    w.u8(msg_type);
    w.u8(frame.ttl);
    w.u16(frame.group_id);
    frame.node.write(&mut w);
    if let Some(state) = state {
        state.write(&mut w);
    }
    debug_assert!(w.len() < MAX_MESSAGE_SIZE, "encoder limit 511 bytes");
    w.into_vec()
}

/// The fixed 20-byte frame prefix (chapter 01 §3), decodable independently
/// of the payload: a receiver answers an Alive with a Response *before*
/// processing the payload and regardless of whether it parses (§4.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub msg_type: u8,
    pub ttl: u8,
    pub group_id: u16,
    pub node: NodeId,
}

impl Header {
    pub fn is_alive(&self) -> bool {
        self.msg_type == TYPE_ALIVE
    }
}

pub fn decode_header(datagram: &[u8]) -> Result<Header> {
    let mut r = Reader::new(datagram);
    if r.remaining() < 20 {
        return Err(Error::Truncated);
    }
    if r.take(8)? != MAGIC {
        return Err(Error::BadMagic);
    }
    Ok(Header {
        msg_type: r.u8()?,
        ttl: r.u8()?,
        group_id: r.u16()?,
        node: NodeId::read(&mut r)?,
    })
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
    let message = match msg_type {
        TYPE_ALIVE => Message::Alive(PeerState::read(r.rest())?),
        TYPE_RESPONSE => Message::Response(PeerState::read(r.rest())?),
        TYPE_BYEBYE => Message::ByeBye,
        other => return Err(Error::UnknownMessageType(other)),
    };
    Ok(Frame {
        ttl,
        group_id,
        node,
        message,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::fourcc;
    use crate::types::Id;

    fn full_state() -> PeerState {
        PeerState {
            timeline: Some(Timeline {
                tempo: 500_000,
                beat_origin: 1_000_000,
                time_origin: 2_500_000,
            }),
            session: Some(Id(*b"SESSIONX")),
            start_stop: Some(StartStopState {
                is_playing: true,
                beats: 4_000_000,
                timestamp: 3_000_000,
            }),
            measurement_endpoint: Some("127.0.0.1:42802".parse().unwrap()),
            audio_endpoint: None,
        }
    }

    #[test]
    fn alive_roundtrip_and_documented_size() {
        let frame = Frame::alive(Id(*b"NODEID01"), full_state());
        let bytes = encode(&frame);
        // Chapter 01 §6.1: plain Link peer state over IPv4 = 107 bytes.
        assert_eq!(bytes.len(), 107);
        assert_eq!(decode(&bytes).unwrap(), frame);
    }

    #[test]
    fn audio_state_is_121_bytes() {
        let mut state = full_state();
        state.audio_endpoint = Some("127.0.0.1:34751".parse().unwrap());
        let bytes = encode(&Frame::alive(Id(*b"NODEID01"), state));
        // Chapter 01 §6.1: LinkAudio-enabled peer state over IPv4 = 121.
        assert_eq!(bytes.len(), 121);
    }

    #[test]
    fn byebye_is_20_bytes() {
        let frame = Frame::bye_bye(Id(*b"NODEID01"));
        let bytes = encode(&frame);
        assert_eq!(bytes.len(), 20);
        assert_eq!(decode(&bytes).unwrap(), frame);
    }

    #[test]
    fn short_or_unmagical_datagrams_are_rejected() {
        assert_eq!(decode(&[0u8; 10]).unwrap_err(), Error::Truncated);
        let mut bytes = encode(&Frame::bye_bye(Id(*b"NODEID01")));
        bytes[0] ^= 0xff;
        assert_eq!(decode(&bytes).unwrap_err(), Error::BadMagic);
    }

    #[test]
    fn unknown_message_type_is_surfaced() {
        let mut bytes = encode(&Frame::bye_bye(Id(*b"NODEID01")));
        bytes[8] = 99;
        assert_eq!(decode(&bytes).unwrap_err(), Error::UnknownMessageType(99));
    }

    #[test]
    fn missing_entries_decode_as_none() {
        let frame = Frame::alive(Id(*b"NODEID01"), PeerState::default());
        let decoded = decode(&encode(&frame)).unwrap();
        assert_eq!(decoded.message, Message::Alive(PeerState::default()));
    }

    #[test]
    fn unknown_payload_entries_are_skipped() {
        let mut w = Writer::new();
        w.bytes(&MAGIC);
        w.u8(TYPE_ALIVE);
        w.u8(5);
        w.u16(0);
        w.id(b"NODEID01");
        w.entry(fourcc(b"zzzz"), |w| w.u32(1234)); // unknown key
        w.entry(keys::SESS, |w| w.id(b"SESSIONX"));
        let frame = decode(&w.into_vec()).unwrap();
        match frame.message {
            Message::Alive(state) => assert_eq!(state.session, Some(Id(*b"SESSIONX"))),
            _ => panic!("expected Alive"),
        }
    }
}
