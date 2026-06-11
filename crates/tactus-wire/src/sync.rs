//! Clock-measurement ping/pong messages (spec chapter 02 §3–§4).

use crate::codec::{Entries, Error, Reader, Result, Writer};
use crate::types::SessionId;

/// Frame magic: ASCII `_link_v` + version byte 0x01 (chapter 02 §3.1).
pub const MAGIC: [u8; 8] = *b"_link_v\x01";
/// Receivers accept up to 512 bytes; senders stay ≤ 511 (chapter 02 §3.1).
pub const MAX_MESSAGE_SIZE: usize = 512;
/// A responder answers pings whose payload is at most this (chapter 02 §4.3).
pub const MAX_PING_PAYLOAD: usize = 32;
/// Ping retry timer (chapter 02 §4.2).
pub const RETRY_MS: u64 = 50;
/// Retries before a measurement fails (chapter 02 §4.2).
pub const MAX_RETRIES: u32 = 5;
/// A measurement completes once more than this many samples are collected
/// (chapter 02 §5).
pub const SAMPLES_REQUIRED: usize = 100;

const TYPE_PING: u8 = 1;
const TYPE_PONG: u8 = 2;

mod keys {
    use crate::codec::fourcc;

    pub const HT: u32 = fourcc(b"__ht"); // initiator host time
    pub const GT: u32 = fourcc(b"__gt"); // responder ghost time
    pub const PGT: u32 = fourcc(b"_pgt"); // previous pong's ghost time
    pub const SESS: u32 = fourcc(b"sess");
}

/// A decoded measurement message. Measurement frames carry no ttl, groupId
/// or NodeId (chapter 02 §3.1) — the conversation is the UDP 5-tuple.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    Ping(Ping),
    Pong(Pong),
}

/// A Ping as the *responder* needs it: the parsed fields plus the verbatim
/// payload bytes, which the pong must echo uninterpreted (chapter 02 §4.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ping {
    /// `__ht`: the initiator's local clock at transmit, µs.
    pub host_time: Option<i64>,
    /// `_pgt`: the previous pong's ghost time, echoed by the initiator.
    pub prev_ghost_time: Option<i64>,
    /// The raw payload (everything after the 9-byte frame prefix).
    pub payload: Vec<u8>,
}

/// A Pong: the responder's own entries plus the echoed ping payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pong {
    /// `sess`: the responder's current session.
    pub session: SessionId,
    /// `__gt`: the responder's ghost clock at pong transmit, µs.
    pub ghost_time: i64,
    /// Verbatim echo of the ping payload (chapter 02 §4.1); parse with
    /// [`parse_echo`] to recover `__ht` / `_pgt`.
    pub echo: Vec<u8>,
}

/// Encode a Ping: `{__ht}` for a fresh/retry ping, `{__ht, _pgt}` in the
/// steady-state chain (chapter 02 §4.1–§4.2).
pub fn encode_ping(host_time: i64, prev_ghost_time: Option<i64>) -> Vec<u8> {
    let mut w = Writer::new();
    w.bytes(&MAGIC);
    w.u8(TYPE_PING);
    w.entry(keys::HT, |w| w.i64(host_time));
    if let Some(pgt) = prev_ghost_time {
        w.entry(keys::PGT, |w| w.i64(pgt));
    }
    w.into_vec()
}

/// Encode a Pong: own `sess` and `__gt` entries, then the ping payload
/// appended verbatim (chapter 02 §4.1, §4.3).
pub fn encode_pong(session: SessionId, ghost_time: i64, echo: &[u8]) -> Vec<u8> {
    let mut w = Writer::new();
    w.bytes(&MAGIC);
    w.u8(TYPE_PONG);
    w.entry(keys::SESS, |w| session.write(w));
    w.entry(keys::GT, |w| w.i64(ghost_time));
    w.bytes(echo);
    w.into_vec()
}

pub fn decode(datagram: &[u8]) -> Result<Message> {
    let mut r = Reader::new(datagram);
    if r.remaining() < 9 {
        return Err(Error::Truncated);
    }
    if r.take(8)? != MAGIC {
        return Err(Error::BadMagic);
    }
    let msg_type = r.u8()?;
    let payload = r.rest();
    match msg_type {
        TYPE_PING => {
            let mut host_time = None;
            let mut prev_ghost_time = None;
            for entry in Entries::new(payload) {
                let entry = entry?;
                match entry.key {
                    keys::HT => host_time = Some(entry.decode(|r| r.i64())?),
                    keys::PGT => prev_ghost_time = Some(entry.decode(|r| r.i64())?),
                    _ => {}
                }
            }
            Ok(Message::Ping(Ping {
                host_time,
                prev_ghost_time,
                payload: payload.to_vec(),
            }))
        }
        TYPE_PONG => {
            // The responder writes its own two entries (sess, __gt) first,
            // then the verbatim echo (chapter 02 §4.1). Decode the leading
            // pair in either order; everything after is the echo.
            let mut entries = Entries::new(payload);
            let mut session = None;
            let mut ghost_time = None;
            let mut echo: &[u8] = &[];
            for _ in 0..2 {
                let rest_before = entries.rest();
                match entries.next() {
                    Some(entry) => {
                        let entry = entry?;
                        match entry.key {
                            keys::SESS => session = Some(entry.decode(SessionId::read)?),
                            keys::GT => ghost_time = Some(entry.decode(|r| r.i64())?),
                            _ => return Err(Error::Malformed("unexpected leading pong entry")),
                        }
                    }
                    None => {
                        echo = rest_before;
                        break;
                    }
                }
                echo = entries.rest();
            }
            match (session, ghost_time) {
                (Some(session), Some(ghost_time)) => Ok(Message::Pong(Pong {
                    session,
                    ghost_time,
                    echo: echo.to_vec(),
                })),
                _ => Err(Error::Malformed("pong missing sess/__gt")),
            }
        }
        other => Err(Error::UnknownMessageType(other)),
    }
}

/// Recover `(__ht, _pgt)` from a pong's echoed payload (chapter 02 §5: PHT
/// and PGT inputs of the offset estimator).
pub fn parse_echo(echo: &[u8]) -> Result<(Option<i64>, Option<i64>)> {
    let mut ht = None;
    let mut pgt = None;
    for entry in Entries::new(echo) {
        let entry = entry?;
        match entry.key {
            keys::HT => ht = Some(entry.decode(|r| r.i64())?),
            keys::PGT => pgt = Some(entry.decode(|r| r.i64())?),
            _ => {}
        }
    }
    Ok((ht, pgt))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Id;

    #[test]
    fn ping_shapes_match_observed_sizes() {
        // Chapter 02 §4.1 [W]: first ping 25 bytes, steady-state ping 41.
        assert_eq!(encode_ping(123, None).len(), 25);
        assert_eq!(encode_ping(123, Some(456)).len(), 41);
    }

    #[test]
    fn pong_shapes_match_observed_sizes() {
        // Chapter 02 §4.1 [W]: first pong 57 bytes, steady-state pong 73.
        let first_ping = encode_ping(123, None);
        let pong = encode_pong(Id(*b"SESSIONX"), 999, &first_ping[9..]);
        assert_eq!(pong.len(), 57);
        let chain_ping = encode_ping(124, Some(999));
        let pong = encode_pong(Id(*b"SESSIONX"), 1000, &chain_ping[9..]);
        assert_eq!(pong.len(), 73);
    }

    #[test]
    fn ping_roundtrip() {
        let bytes = encode_ping(-5, Some(7));
        match decode(&bytes).unwrap() {
            Message::Ping(p) => {
                assert_eq!(p.host_time, Some(-5));
                assert_eq!(p.prev_ghost_time, Some(7));
                assert_eq!(p.payload, &bytes[9..]);
            }
            _ => panic!("expected ping"),
        }
    }

    #[test]
    fn pong_roundtrip_preserves_echo() {
        let ping = encode_ping(11, Some(22));
        let bytes = encode_pong(Id(*b"SESSIONX"), 33, &ping[9..]);
        match decode(&bytes).unwrap() {
            Message::Pong(p) => {
                assert_eq!(p.session, Id(*b"SESSIONX"));
                assert_eq!(p.ghost_time, 33);
                assert_eq!(p.echo, &ping[9..]);
                assert_eq!(parse_echo(&p.echo).unwrap(), (Some(11), Some(22)));
                assert_eq!(encode_pong(p.session, p.ghost_time, &p.echo), bytes);
            }
            _ => panic!("expected pong"),
        }
    }

    #[test]
    fn empty_ping_payload_is_valid() {
        // A responder answers any ping with payload ≤ 32 bytes (§4.3),
        // including an empty one.
        let mut w = Writer::new();
        w.bytes(&MAGIC);
        w.u8(TYPE_PING);
        match decode(&w.into_vec()).unwrap() {
            Message::Ping(p) => {
                assert_eq!(p.host_time, None);
                assert!(p.payload.is_empty());
            }
            _ => panic!("expected ping"),
        }
    }
}
