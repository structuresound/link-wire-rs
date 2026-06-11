//! Wire codec for the Link session protocol family, implemented clean-room
//! from the published specification
//! ([link-wire-spec](https://github.com/structuresound/link-wire-spec)).
//!
//! Three UDP protocols share one serialization scheme (spec chapter 00):
//!
//! - [`discovery`] — multicast peer discovery and state gossip (chapter 01)
//! - [`sync`] — unicast ping/pong clock measurement (chapter 02)
//! - [`audio`] — LinkAudio v1 channel control and audio streaming (chapter 03)
//!
//! This crate contains pure encode/decode logic only: no sockets, no clocks,
//! no state machines. Every message decoded from the spec's golden packet
//! captures re-encodes byte-for-byte (see `tests/vectors.rs`).

pub mod audio;
pub mod codec;
pub mod discovery;
pub mod sync;
pub mod types;

pub use codec::{Error, Result};
