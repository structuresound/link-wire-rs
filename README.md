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

## Status

**Governance only — no code yet.** Implementation work happens in sessions
and environments that have never had access to the reference source, starting
from spec release 0.1.0. The conformance harness (which builds and runs
upstream reference binaries as interop test peers — use of GPL software, not
distribution) will live under `conformance/` and is the project's definition
of done:

1. Byte-level: round-trip the spec's golden packet captures.
2. Behavioral: join a session with a reference peer on loopback; converge
   tempo and align beat phase within tolerance.
3. Audio: exchange PCM with a reference LinkAudio peer with correct
   beat-time alignment.
4. Canary: the same suite against upstream HEAD, as a protocol-drift tripwire.

## Affiliation

This project is not affiliated with, endorsed by, or sponsored by Ableton AG.
"Ableton", "Link", and "Ableton Link" are trademarks of Ableton AG, used here
only to describe interoperability. This implementation does not use the Link
name or badge as branding.
