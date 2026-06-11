//! Shared protocol value types (spec chapter 00 §2, §4.7–§4.9).

use std::fmt;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};

use crate::codec::{Error, Reader, Result, Writer};

/// 8-byte random identifier: NodeId / SessionId / ChannelId (chapter 00 §4.8).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Id(pub [u8; 8]);

impl Id {
    pub fn read(r: &mut Reader<'_>) -> Result<Id> {
        Ok(Id(r.id()?))
    }

    pub fn write(&self, w: &mut Writer) {
        w.id(&self.0);
    }
}

impl fmt::Debug for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for b in self.0 {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

/// A peer instance on the network (chapter 00 §2).
pub type NodeId = Id;
/// A session: the NodeId of the peer that founded it (chapter 00 §2).
pub type SessionId = Id;
/// A published audio channel (chapter 00 §2).
pub type ChannelId = Id;

/// The session timeline: a bijection between beats and ghost-time
/// microseconds (chapter 02 §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timeline {
    /// Tempo as a period: microseconds per beat (chapter 00 §4.7).
    pub tempo: i64,
    /// Beat origin in micro-beats.
    pub beat_origin: i64,
    /// Time origin in ghost-time microseconds.
    pub time_origin: i64,
}

impl Timeline {
    pub const WIRE_SIZE: usize = 24;

    pub fn read(r: &mut Reader<'_>) -> Result<Timeline> {
        Ok(Timeline {
            tempo: r.i64()?,
            beat_origin: r.i64()?,
            time_origin: r.i64()?,
        })
    }

    pub fn write(&self, w: &mut Writer) {
        w.i64(self.tempo);
        w.i64(self.beat_origin);
        w.i64(self.time_origin);
    }
}

/// Transport start/stop state (chapter 02 §8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StartStopState {
    pub is_playing: bool,
    /// Session-timeline beat position of the transport change, micro-beats.
    pub beats: i64,
    /// Ghost time of the user action, microseconds.
    pub timestamp: i64,
}

impl StartStopState {
    pub const WIRE_SIZE: usize = 17;

    pub fn read(r: &mut Reader<'_>) -> Result<StartStopState> {
        Ok(StartStopState {
            is_playing: r.bool()?,
            beats: r.i64()?,
            timestamp: r.i64()?,
        })
    }

    pub fn write(&self, w: &mut Writer) {
        w.bool(self.is_playing);
        w.i64(self.beats);
        w.i64(self.timestamp);
    }
}

/// Read the 6-byte IPv4 endpoint value layout (chapter 00 §4.9).
pub fn read_endpoint_v4(r: &mut Reader<'_>) -> Result<SocketAddr> {
    let addr = Ipv4Addr::from(r.u32()?);
    let port = r.u16()?;
    Ok(SocketAddr::V4(SocketAddrV4::new(addr, port)))
}

/// Read the 18-byte IPv6 endpoint value layout. No scope (zone) identifier
/// is transmitted; receivers substitute the arrival interface's scope
/// (chapter 01 §6) — here it decodes as 0.
pub fn read_endpoint_v6(r: &mut Reader<'_>) -> Result<SocketAddr> {
    let addr = Ipv6Addr::from(u128::from_be_bytes(r.take(16)?.try_into().unwrap()));
    let port = r.u16()?;
    Ok(SocketAddr::V6(SocketAddrV6::new(addr, port, 0, 0)))
}

/// Write an endpoint in its family's value layout.
pub fn write_endpoint(w: &mut Writer, ep: &SocketAddr) {
    match ep {
        SocketAddr::V4(v4) => {
            w.u32(u32::from(*v4.ip()));
            w.u16(v4.port());
        }
        SocketAddr::V6(v6) => {
            w.bytes(&v6.ip().octets());
            w.u16(v6.port());
        }
    }
}

/// Write the endpoint payload entries for one endpoint kind: exactly one of
/// the v4/v6 keys, matching the endpoint's family; nothing when absent
/// (chapter 01 §6 "family switch", chapter 00 §4.5 rule 5).
pub fn write_endpoint_entry(w: &mut Writer, key_v4: u32, key_v6: u32, ep: &Option<SocketAddr>) {
    if let Some(ep) = ep {
        let key = match ep {
            SocketAddr::V4(_) => key_v4,
            SocketAddr::V6(_) => key_v6,
        };
        w.entry(key, |w| write_endpoint(w, ep));
    }
}

/// Decode an endpoint entry value by its declared size (6 = IPv4, 18 = IPv6).
pub fn decode_endpoint_value(value: &[u8]) -> Result<SocketAddr> {
    let mut r = Reader::new(value);
    let ep = match value.len() {
        6 => read_endpoint_v4(&mut r)?,
        18 => read_endpoint_v6(&mut r)?,
        _ => return Err(Error::Malformed("endpoint value size")),
    };
    debug_assert!(r.is_empty());
    Ok(ep)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::Writer;

    #[test]
    fn endpoint_v4_roundtrip() {
        let ep: SocketAddr = "192.168.77.1:20808".parse().unwrap();
        let mut w = Writer::new();
        write_endpoint(&mut w, &ep);
        let buf = w.into_vec();
        assert_eq!(buf.len(), 6);
        assert_eq!(decode_endpoint_value(&buf).unwrap(), ep);
    }

    #[test]
    fn endpoint_v6_roundtrip() {
        let ep: SocketAddr = "[fe80::1234]:9000".parse().unwrap();
        let mut w = Writer::new();
        write_endpoint(&mut w, &ep);
        let buf = w.into_vec();
        assert_eq!(buf.len(), 18);
        assert_eq!(decode_endpoint_value(&buf).unwrap(), ep);
    }
}
