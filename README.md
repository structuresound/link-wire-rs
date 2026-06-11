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
keeps — a tempo-sync library name with no Link branding. The names are
confirmed; first publication to crates.io is pending (both verified
packageable with `cargo package --workspace`).

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

## Testing and conformance

Three layers, from hermetic to interop:

1. **Unit + golden vectors** — `cargo test --workspace`. The vector tests
   need a checkout of link-wire-spec, found via `LINK_WIRE_SPEC_DIR` or a
   sibling `../link-wire-spec` directory; they skip (with a notice) when
   absent, and CI sets `REQUIRE_VECTORS=1` so absence fails there. Includes
   the candidate-vs-candidate contract run (two `conformance-peer`
   processes driven through every harness scenario), so the full suite
   passes inside the clean room with no reference software involved.
2. **CI** ([ci.yml](.github/workflows/ci.yml)) — fmt, clippy, the test
   suite against the pinned spec release (`SPEC_REF`), and
   `cargo deny check bans licenses sources` (the dependency-level firewall;
   see [deny.toml](deny.toml)).
3. **Reference interop** ([conformance.yml](.github/workflows/conformance.yml))
   — runs the spec repo's [conformance harness](https://github.com/structuresound/link-wire-spec/tree/main/conformance)
   against `conformance-peer`: reference and candidate side by side on
   loopback in an isolated network namespace, scenarios
   `discovery-join-leave`, `tempo-follow`, `start-stop`, `beat-alignment`,
   and `audio-stream`. The observation log is uploaded as the
   `conformance-observations` artifact.

To run the reference interop locally (requires Linux, root for the network
namespace, and the build/JACK packages listed in the workflow):

```sh
cargo build --release -p tactus --bin conformance-peer
git clone https://github.com/structuresound/link-wire-spec /tmp/link-wire-spec
git -C /tmp/link-wire-spec checkout <SPEC_REF from the workflow>
export CANDIDATE_CMD="$PWD/target/release/conformance-peer"
export CANDIDATE_FEATURES=audio
sudo --preserve-env=CANDIDATE_CMD,CANDIDATE_FEATURES \
  bash /tmp/link-wire-spec/conformance/run-isolated.sh   # [scenario ...]
```

**Firewall rules for that run (PROVENANCE.md):** the harness clones and
builds the GPL reference into a work directory *outside* this repository —
never vendor it, never open files under its cache, and treat the `OBS |`
lines as the only output you act on. A clean-room authoring session must
not run it at all (it has no github.com access by design); consume the CI
artifact instead. Anyone who *has* read reference source is permanently
"dirty" and may contribute observations, not code.

## Affiliation

This project is not affiliated with, endorsed by, or sponsored by Ableton AG.
"Ableton", "Link", and "Ableton Link" are trademarks of Ableton AG, used here
only to describe interoperability. This implementation does not use the Link
name or badge as branding.
