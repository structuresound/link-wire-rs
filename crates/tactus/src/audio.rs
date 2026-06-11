//! LinkAudio v1 runtime (spec chapter 03). Implemented in milestone M3;
//! these hooks keep the engine's call sites stable.

use std::net::SocketAddr;

use tactus_wire::types::NodeId;

use crate::engine::{Engine, State};

/// Per-peer LinkAudio runtime state. Present iff LinkAudio is enabled, which
/// is also what switches the `aep4` advertisement on (chapter 03 §2).
pub struct AudioState {}

pub fn handle_datagram(_eng: &Engine, _st: &mut State, _gw: usize, _src: SocketAddr, _buf: &[u8]) {}

pub fn peer_left(_eng: &Engine, _st: &mut State, _node: NodeId) {}

pub fn housekeeping(_eng: &Engine, _st: &mut State, _now: i64) -> i64 {
    i64::MAX
}

pub fn shutdown(_eng: &Engine, _st: &mut State) {}
