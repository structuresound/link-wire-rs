//! Common serialization rules (spec chapter 00 §4): big-endian primitives
//! written back-to-back, length-prefixed strings and vectors, and the tagged
//! key-value payload container.

use std::fmt;

/// Decode failure. Per spec chapter 00 §4.10 a failed message is discarded
/// by the receiver; the error carries enough detail for diagnostics only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// A primitive read would run past the end of the available bytes.
    Truncated,
    /// The datagram does not start with the expected 8-byte frame magic.
    BadMagic,
    /// Message type not defined by the protocol chapter being decoded.
    UnknownMessageType(u8),
    /// Structurally invalid content (reason is a static description).
    Malformed(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Truncated => write!(f, "read past end of buffer"),
            Error::BadMagic => write!(f, "bad frame magic"),
            Error::UnknownMessageType(t) => write!(f, "unknown message type {t}"),
            Error::Malformed(why) => write!(f, "malformed message: {why}"),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

/// The big-endian `u32` formed by four ASCII bytes — payload entry keys
/// (spec chapter 00 §4.5).
pub const fn fourcc(s: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*s)
}

/// Sequential reader over a byte slice. Every read is bounds-checked; a read
/// past the end is [`Error::Truncated`] (chapter 00 §4.1).
#[derive(Clone)]
pub struct Reader<'a> {
    buf: &'a [u8],
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Reader { buf }
    }

    pub fn remaining(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// The unread remainder of the buffer, without consuming it.
    pub fn rest(&self) -> &'a [u8] {
        self.buf
    }

    pub fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if n > self.buf.len() {
            return Err(Error::Truncated);
        }
        let (head, tail) = self.buf.split_at(n);
        self.buf = tail;
        Ok(head)
    }

    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    pub fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into().unwrap()))
    }

    pub fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }

    pub fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_be_bytes(self.take(8)?.try_into().unwrap()))
    }

    pub fn i16(&mut self) -> Result<i16> {
        Ok(self.u16()? as i16)
    }

    pub fn i32(&mut self) -> Result<i32> {
        Ok(self.u32()? as i32)
    }

    pub fn i64(&mut self) -> Result<i64> {
        Ok(self.u64()? as i64)
    }

    /// `bool`: 0 = false, any nonzero decodes as true (chapter 00 §4.1).
    pub fn bool(&mut self) -> Result<bool> {
        Ok(self.u8()? != 0)
    }

    /// Fixed 8-byte identifier (chapter 00 §4.3/§4.8).
    pub fn id(&mut self) -> Result<[u8; 8]> {
        Ok(self.take(8)?.try_into().unwrap())
    }

    /// Length-prefixed string (chapter 00 §4.2). A declared length greater
    /// than the remaining bytes is a parse error [N].
    pub fn string(&mut self) -> Result<&'a [u8]> {
        let n = self.u32()? as usize;
        self.take(n)
    }
}

/// Append-only writer producing the wire encoding.
#[derive(Default)]
pub struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    pub fn new() -> Self {
        Writer::default()
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn into_vec(self) -> Vec<u8> {
        self.buf
    }

    pub fn bytes(&mut self, b: &[u8]) {
        self.buf.extend_from_slice(b);
    }

    pub fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }

    pub fn u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    pub fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    pub fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    pub fn i16(&mut self, v: i16) {
        self.u16(v as u16);
    }

    pub fn i32(&mut self, v: i32) {
        self.u32(v as u32);
    }

    pub fn i64(&mut self, v: i64) {
        self.u64(v as u64);
    }

    /// Encoders write 0 or 1 (chapter 00 §4.1).
    pub fn bool(&mut self, v: bool) {
        self.u8(v as u8);
    }

    pub fn id(&mut self, v: &[u8; 8]) {
        self.bytes(v);
    }

    pub fn string(&mut self, s: &[u8]) {
        self.u32(s.len() as u32);
        self.bytes(s);
    }

    /// Write one payload-container entry (chapter 00 §4.5): key, value size,
    /// value bytes produced by `f`.
    pub fn entry(&mut self, key: u32, f: impl FnOnce(&mut Writer)) {
        self.u32(key);
        let size_at = self.buf.len();
        self.u32(0);
        f(self);
        let size = (self.buf.len() - size_at - 4) as u32;
        self.buf[size_at..size_at + 4].copy_from_slice(&size.to_be_bytes());
    }
}

/// One payload-container entry (chapter 00 §4.5).
#[derive(Debug)]
pub struct Entry<'a> {
    /// Four-character code as a big-endian `u32`.
    pub key: u32,
    /// The entry value, exactly `size` bytes.
    pub value: &'a [u8],
}

impl Entry<'_> {
    /// Decode the value with `f`, requiring that the declared size is
    /// exactly consumed (chapter 00 §4.5 rule 4).
    pub fn decode<T>(&self, f: impl FnOnce(&mut Reader<'_>) -> Result<T>) -> Result<T> {
        let mut r = Reader::new(self.value);
        let v = f(&mut r)?;
        if !r.is_empty() {
            return Err(Error::Malformed("entry value not exactly consumed"));
        }
        Ok(v)
    }
}

/// Iterator over the entries of a payload container. An entry whose declared
/// size extends past the end of the payload is a parse error for the whole
/// payload (chapter 00 §4.5 rule 3); unknown keys are the caller's to skip
/// (rule 2).
pub struct Entries<'a> {
    r: Reader<'a>,
}

impl<'a> Entries<'a> {
    pub fn new(payload: &'a [u8]) -> Self {
        Entries {
            r: Reader::new(payload),
        }
    }

    /// Bytes not yet consumed (the suffix starting at the next entry).
    pub fn rest(&self) -> &'a [u8] {
        self.r.rest()
    }
}

impl<'a> Iterator for Entries<'a> {
    type Item = Result<Entry<'a>>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.r.is_empty() {
            return None;
        }
        let parse = (|| {
            let key = self.r.u32()?;
            let size = self.r.u32()? as usize;
            let value = self.r.take(size)?;
            Ok(Entry { key, value })
        })();
        Some(parse)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fourcc_matches_spec_table() {
        // Chapter 00 §4.5 gives 'sess' = 0x73657373.
        assert_eq!(fourcc(b"sess"), 0x7365_7373);
    }

    #[test]
    fn primitive_reads_are_bounds_checked() {
        let mut r = Reader::new(&[0x01, 0x02]);
        assert_eq!(r.u16().unwrap(), 0x0102);
        assert_eq!(r.u8(), Err(Error::Truncated));
    }

    #[test]
    fn string_length_past_end_is_error() {
        // N = 5 but only 2 bytes remain (chapter 00 §4.2 [N]).
        let mut r = Reader::new(&[0, 0, 0, 5, b'a', b'b']);
        assert_eq!(r.string(), Err(Error::Truncated));
    }

    #[test]
    fn entry_size_past_end_is_error() {
        let mut w = Writer::new();
        w.u32(fourcc(b"test"));
        w.u32(10); // declares 10 value bytes...
        w.bytes(&[1, 2, 3]); // ...but only 3 follow
        let buf = w.into_vec();
        let mut it = Entries::new(&buf);
        assert_eq!(it.next().unwrap().unwrap_err(), Error::Truncated);
    }

    #[test]
    fn entry_roundtrip() {
        let mut w = Writer::new();
        w.entry(fourcc(b"__ht"), |w| w.i64(-42));
        let buf = w.into_vec();
        assert_eq!(buf.len(), 16);
        let mut it = Entries::new(&buf);
        let e = it.next().unwrap().unwrap();
        assert_eq!(e.key, fourcc(b"__ht"));
        assert_eq!(e.decode(|r| r.i64()).unwrap(), -42);
        assert!(it.next().is_none());
    }

    #[test]
    fn entry_value_must_be_exactly_consumed() {
        let mut w = Writer::new();
        w.entry(fourcc(b"xxxx"), |w| w.u32(7));
        let buf = w.into_vec();
        let e = Entries::new(&buf).next().unwrap().unwrap();
        // Decoding fewer bytes than declared is a parse error (rule 4).
        assert!(e.decode(|r| r.u16()).is_err());
    }
}
