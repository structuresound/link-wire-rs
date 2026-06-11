# link-wire-rs

A clean-room Rust implementation of the wire protocols documented in
[link-wire-spec](https://github.com/structuresound/link-wire-spec):
peer discovery, tempo/beat synchronization, and LinkAudio v1 audio sharing —
wire-compatible with applications using [Ableton Link](https://ableton.github.io/link/).

**License: MIT.** This is the point of the project: a permissively licensed,
embeddable implementation, suitable for closed-source applications, built
*exclusively* from the published protocol specification — never from the
GPL reference source. The rules that make that claim auditable are in
[PROVENANCE.md](PROVENANCE.md). Read it before contributing; PRs certify
compliance with it.

## Crates

| Crate | Contents |
|---|---|
| [`tactus-wire`](crates/tactus-wire) | Pure wire codec: common serialization (spec ch. 00) and encode/decode for every discovery, sync, and LinkAudio v1 message (ch. 01–03). No sockets, no state. |
| [`tactus`](crates/tactus) | The runtime peer: discovery gossip, clock measurement, session election, timeline/start-stop sync, LinkAudio sinks and sources. |

*Tactus* is the early-music term for the shared steady beat an ensemble
keeps — a tempo-sync library name with no Link branding. Neither crate is
published yet; the names are reserved proposals pending confirmation.

## Status

Implementation work happens in sessions and environments that have never had
access to the reference source (see [docs/CLEAN-TEAM.md](docs/CLEAN-TEAM.md)),
built against spec release 0.4.0. The conformance harness (which builds and
runs upstream reference binaries as interop test peers — use of GPL software,
not distribution) integrates via CI caches only. The project's definition of
done, by milestone:

1. **M1 — done.** Byte-level: every packet in the spec's golden captures
   (4,346 datagrams across five scenarios) decodes and re-encodes
   byte-for-byte (`crates/tactus-wire/tests/vectors.rs`).
2. **M2 — done (self-interop).** Behavioral: discovery, session election,
   tempo convergence, beat-phase alignment, start/stop, churn and ttl
   expiry, two peers on loopback (`crates/tactus/tests/loopback.rs`).
3. **M3 — done (self-interop).** Audio: channel lifecycle, PCM i16
   streaming with beat-time alignment within 0.001 beat
   (`crates/tactus/tests/audio_loopback.rs`).
4. **M4 — wired.** `conformance-peer` (the
   [CANDIDATE-CONTRACT](https://github.com/structuresound/link-wire-spec/blob/main/conformance/CANDIDATE-CONTRACT.md)
   binary) is exercised end to end candidate-vs-candidate in
   `crates/tactus/tests/conformance_contract.rs`; the
   [conformance workflow](.github/workflows/conformance.yml) runs the spec
   repo's harness against reference peers on loopback in CI. Canary against
   upstream HEAD is future work.

## Affiliation

This project is not affiliated with, endorsed by, or sponsored by Ableton AG.
"Ableton", "Link", and "Ableton Link" are trademarks of Ableton AG, used here
only to describe interoperability. This implementation does not use the Link
name or badge as branding.
