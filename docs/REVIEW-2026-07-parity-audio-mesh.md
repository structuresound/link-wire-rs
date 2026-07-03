# Architecture review — parity, tactus audio, and mesh

**Date:** 2026-07-03
**Scope:** `link-wire-rs` (implementation) + `link-wire-spec` (spec) + candidate
mesh substrates (`mshr`, `cheers` lan-pair, `iroh`).
**Goal:** a prioritized, delegable backlog to take link-wire from a
byte-accurate conformance peer to (1) a real-LAN Ableton/LinkAudio-parity node
and (2) the "tactus native" future — better audio and a topology-aware mesh —
without breaking the clean-room provenance or the v1 interop floor.

Spec-side actionables have a companion doc:
[`link-wire-spec/REVIEW-2026-07-spec-backlog.md`](../../link-wire-spec/REVIEW-2026-07-spec-backlog.md).

---

## TL;DR

The core is in good shape and honestly tested: two crates (`tactus-wire` a
zero-dep codec, `tactus` the runtime), ~5k LOC, 44 tests, all five golden pcaps
round-tripped byte-for-byte in CI, milestone claims that match reality. The
work ahead is not correctness rework — it is **reach, real-time fitness, and
three abstractions the native roadmap needs before features pile on**.

Three things are load-bearing and should land first, because most of Lanes B
and C depend on them:

1. **Real network reach** (interface enumeration + IPv6). Today the default is
   loopback-only; this blocks parity on an actual LAN. *(Lane A)*
2. **A forward-compatible codec** (`#[non_exhaustive]` + raw-entry
   preservation). Every `tcap`/native addition is a semver break and a lossy
   re-encode until this lands. *(Lane B prerequisite)*
3. **A gateway trait** (interface gateways + a virtual overlay gateway behind
   one interface). The mesh overlay is "just another gateway" in the spec's
   own framing — but `Gateway` is a concrete 4-socket struct today. *(Lane C
   prerequisite)*

Substrate decisions (detail in [§Substrates](#substrates)): **wrap `mshr`** for
the authenticated QUIC overlay, **fork-minimal the `cheers` lan-pair pattern**
(not the crate) for stage pairing, reach **`iroh` only through `mshr`**. The
overlay/mesh code must live in a **separate workspace** (`tactus-mesh`) — `mshr`'s
dependency tree carries copyleft licenses that fail `link-wire-rs`'s `deny.toml`,
and the spec's own layering wants the split anyway.

**Trust model** (the part the owner cares most about, [§Trust](#trust-model)):
*identity is constant, authentication is tiered*. Every peer always has a stable
Ed25519 mesh identity and the overlay is always encrypted+authenticated — what
changes between **jam / adhoc / stage** is admission *policy*, not crypto.
"Lockdown" is a policy-epoch flip, not a re-handshake.

---

## Current state — what's already right

Worth stating so the backlog doesn't read as a list of failures. These are load-
bearing strengths to preserve:

- **The wire crate is a clean, total codec.** `tactus-wire` has zero
  dependencies, bounds-checks every read, and models chapters 00–03 precisely.
  Malformed input is dropped, not panicked on.
- **Tests are honest and CI-backed.** 44 tests; `tests/vectors.rs` decodes and
  re-encodes **every** UDP payload in all five spec pcaps byte-for-byte (pinned
  count 4,346) and CI fails if the vectors are absent. M1–M3 milestone claims
  in the README match what the code does; M4 conformance is wired to a
  reference-vs-candidate harness in an isolated netns.
- **Spec fidelity is high and self-documented.** Nearly every code block cites
  the chapter/§ it implements. The deviations found in this review are few and
  specific (see A4, below), not systemic.
- **The clean-room discipline is real.** `PROVENANCE.md` + `deny.toml` enforce a
  license firewall and a one-way clean/dirty split. This is an asset; the
  backlog is careful to preserve it (e.g. the license reason the mesh crate
  lives outside this workspace).

The risks are concentrated in three places — platform reach, real-time
architecture, and the absence of the abstractions the native roadmap needs —
and that is exactly how the lanes below are organized.

---

## How to read the backlog

Each ticket carries:

- **ID** — `A1`, `B2`, … stable within this doc.
- **Size** — S (hours), M (a day or two), L (multi-day / needs design).
- **Repo** — `rs` = link-wire-rs, `spec` = link-wire-spec, `mesh` = the new
  `tactus-mesh` workspace.
- **Delegable?** — 🟢 Sonnet-implementable from the seam given; 🟡 needs a design
  decision first (flagged in [§Decisions](#decisions-that-need-a-human)); 🔴
  needs a human/owner call before any code.
- **Seam** — the concrete file:line to start from.

---

## Lane A — Parity hardening

Make link-wire a real node on a real network, and close the handful of spec
deviations. This lane is almost entirely 🟢 — well-specified work against a
closed normative core.

### A1 — Interface enumeration + gateway lifecycle · L · rs · 🟢
The #1 parity gap. `Config::gateways` is a manual `Vec<Ipv4Addr>` defaulting to
loopback (`crates/tactus/src/lib.rs:38-51`; admitted at `lib.rs:10-13`).
Ableton parity requires enumerating live interfaces, joining the discovery group
per interface, and rescanning on the reference's ~5 s cadence (spec
`01-discovery.md:70-71`) so gateways open/close as interfaces come and go.
**Seam:** `crates/tactus/src/net.rs` (gateway construction), `runtime.rs:111-117`
(gateway spawn; note it currently `eprintln!`s and continues on open failure, so
`enable()` can silently bind zero gateways — fix as part of this).
**Note:** land *after* A6 (gateway trait) so enumeration produces trait objects,
not concrete structs that then get refactored.

### A2 — IPv6 link-local gateways · M · rs · 🟢
The wire codec already encodes/decodes v6 endpoints (`types.rs:112-116`), but no
v6 socket path exists at runtime. Spec `01-discovery.md:72-76` defines the v6
gateway + link-local filter rule. Blocks parity on v6-primary networks and pairs
with the spec's only open evidence gap (D5 / `discovery-ipv6.pcap`).
**Seam:** `net.rs` socket construction; mirror the IPv4 multicast discipline for
`ff12::8080`.

### A3 — Enforce receive/encode size caps in release · S · rs · 🟢
`MAX_RECV_PAYLOAD` (1180) is defined but never enforced on decode
(`tactus-wire/src/audio.rs:13`); encode caps are `debug_assert!` only
(`audio.rs:234`, `discovery.rs:162`) and length fields are cast `as u16`/`as u32`
unchecked — a misbehaving caller emits silently-truncated (corrupt) frames in
release builds. Convert caps to real `Result` errors on encode; bound decode by
the declared cap, not the 2048-byte socket buffer (`runtime.rs:47`).
**Seam:** `tactus-wire/src/audio.rs:13,234`; `discovery.rs:162`.

### A4 — Path selection: spec §4.2 hysteresis + stable keying · M · rs · 🟢
`best_path` (`crates/tactus/src/audio.rs:158-168`) recomputes `max_by` on every
send with **no stickiness**, and equal-quality ties fall to nondeterministic
`HashMap` iteration order → the path can flap and reorder across gateways. Spec
`03-audio.md` §4.2 requires "an existing path is replaced only by a *strictly
better* one." Two fixes: (a) add the strictly-better hysteresis (keep the
current path unless a challenger is strictly higher quality); (b) key the RTT
window by destination endpoint, not `(NodeId, gateway)`, so stale stats don't
survive an endpoint change. **This directly underpins the mesh** — the proposal's
§8.3 anti-oscillation story reuses exactly this rule, so getting it right here
pays twice. **Seam:** `audio.rs:66-92` (`PathStats`), `audio.rs:158-168`
(`best_path`), keys at `audio.rs:29,39`.

### A5 — Sink tail flush · S · rs · 🟢
`write_sink` transmits only *full* datagrams; sub-datagram remainders sit in
`segments` until more audio arrives, so end-of-stream audio is silently held or
dropped and there is no way to force it out. Add a `flush_channel` API (and flush
on `unpublish`). **Seam:** `crates/tactus/src/audio.rs:484-490`.

### A6 — Gateway trait (interface + overlay) · L · rs · 🟢 (design in D2)
`Gateway` is a concrete struct of four `UdpSocket`s (`net.rs:19-31`) reached
directly from three modules (`engine.rs`, `audio.rs`, `runtime.rs`). The mesh
overlay (Lane C) needs to appear as one more gateway. **Good news:** the seams
already exist — gateways are index-addressed everywhere (`State.gateways:
Vec<Arc<Gateway>>`, peers keyed `(NodeId, gw_idx)`), and the inbound funnel
`dispatch(eng, st, gw_idx, src, buf, role)` (`runtime.rs:72-80`) is
transport-agnostic. **The work:** trait-ify `Gateway` with role-addressed sends
(`send_discovery`, `send_measurement`, `send_audio`) + a receiver-spawn hook;
generalize `Config` beyond `Vec<Ipv4Addr>`; and decide the endpoint
representation (today `SocketAddr` end-to-end — an overlay peer needs either a
synthetic `SocketAddr` mapping or an abstracted runtime endpoint type; the *wire*
type stays `SocketAddr` for v1 compat). Discovery over an overlay gateway is
unicast/gossip, not multicast, so the trait must express an alive-variant.
**This is a prerequisite for Lane C** and should land before native features
touch the concrete type. **Seam:** `net.rs:19-31`, `runtime.rs:72-80`,
`lib.rs:38-51`.

---

## Lane B — Better audio

The native audio wins, ordered cheapest-first. The §-refs are to
`tactus-native-audio.md`. The first item ships value with *zero* protocol change;
the rest are gated on a forward-compatible codec (B2).

### B1 — Zero-negotiation datagram packing · M · rs · 🟢
Spec §3: a sender may fill the 1200-byte datagram (vs the reference's 502) as
long as **every chunk stays ≤512 frames** — ~2.3× fewer packets, legal to send
to a *reference* peer today. The whole sender policy is the `write_sink` drain
loop (`crates/tactus/src/audio.rs:484-525`): replace the single 502-byte budget
with a datagram byte budget + a **new explicit ≤512-frame-per-chunk constant**.
That constant does not exist today — the current safety is an accident of the
502-byte cap (a single 550-frame chunk would abort a reference renderer per spec
§5.9). Add it next to `SAMPLE_BYTE_CAP` in `tactus-wire/src/audio.rs:15` and make
it load-bearing. **Seam:** `audio.rs:484-525`; new const in `tactus-wire`.

### B2 — Forward-compatible codec: `#[non_exhaustive]` + raw-entry preservation · M · rs · 🟢 (prerequisite)
The `Message` enums in `tactus-wire` are exhaustive and **decode discards
unknown payload entries**, so a frame carrying entries the codec doesn't model
won't re-encode byte-for-byte. That is fine for today's vectors (none carry
extras) but is a **semver break + lossy round-trip for every `tcap`/native
addition**. Fix before B3+: mark the public enums `#[non_exhaustive]`, and give
decode a raw-entry escape hatch (preserve unknown `Entry`s so decode→encode is
lossless). This is the single change that makes the rest of Lane B additive
instead of breaking. **Seam:** `tactus-wire/src/audio.rs` (`Message` enums,
`PeerAnnouncement`/`ChannelRequest` decode loops ~`:266-274,:321`).

### B3 — `tcap` capability TLV on announcement + channel request · M · rs · 🟢 (needs D3 registry)
The mechanical half of native capability negotiation. Add a `tcap` payload entry
(TLV: codecs / max-chunk-frames / multicast-group / FEC-schemes / clock-domains /
latency-target) to `PeerAnnouncement` and `ChannelRequest`; the payload container
already skips unknown keys so a reference peer ignores it (spec §1.1, §4.1-4.2).
Record native-capable peers in `AudioState.known`/`paths`
(`crates/tactus/src/audio.rs:33-42`). Blocked on B2 (else it is a breaking
change) and on the spec pinning the fourcc + TLV numbers (D3 / spec ticket).
**Seam:** encode sites `tactus-wire/src/audio.rs:180-214`.

### B4 — Native data-plane message type (16) · M · rs · 🟢 (needs D4 grammar)
Type dispatch is a single match (`tactus-wire/src/audio.rs:251-283`); add
`TYPE_NATIVE_AUDIO = 16` + a `Message` variant + a runtime arm in
`handle_datagram`. The receive path (beat mapping via `deliver`) is reusable —
native chunks keep beat-time semantics. The real work is **sender-side
per-requester format choice**: `write_sink` currently encodes one datagram set
and fans it out (`audio.rs:529-536`); native mode must encode per-requester (v1
for reference requesters, native for `tcap` requesters) keyed by B3's knowledge.
Blocked on B2 + B3 and on the spec grammar (D4). **Seam:** `audio.rs:251-283`,
`crates/tactus/src/audio.rs:199-271,529-536`.

### B5 — Loss measurement · M · rs · 🟢
Prerequisite for FEC being *evaluable*, and a gap today: the data plane is
open-loop (spec §5.8) and the inbox silently drops oldest beyond 256 chunks
(`audio.rs:292-295`); sequence numbers exist but nothing counts gaps. Add a
per-channel loss/reorder counter and surface it (beyond the 1 s `is_receiving`
boolean). Cheap, and it turns "better robustness" from an assertion into a
measurement. **Seam:** `crates/tactus/src/audio.rs:274-296`.

### B6 — FEC (XOR-parity first, RaptorQ later) · L · rs+mesh · 🔴 (design)
The headline native robustness win (spec §5). XOR-parity across a sequence window
is the small first step; RaptorQ is the heavier option. Gated on B4 (native data
plane) + B5 (so you can measure that it helps) + the spec pinning the FEC framing
(D4). Keep the codec in `tactus-wire`; the *scheme selection* is a `tcap`
negotiation. **Seam:** new module under `tactus`, framing const in `tactus-wire`.

### B7 — Real-time audio path (lock split + allocation discipline) · L · rs · 🔴 (design)
The ceiling on "better audio" as a *live* node. One global `Mutex<State>`
(`engine.rs:110-116`) spans the audio hot path — the app's `write_channel` /
`poll_channel` contend with every network receiver thread and the housekeeping
pass — and the send/receive paths do 3–4 copies + multiple `Vec` allocations per
datagram (`audio.rs:472-524`, `:274-296`). Fine for a conformance peer, unfit for
an RT audio callback (priority inversion + unbounded allocation = jitter). Split
the app-facing audio API from the network core with SPSC rings; preallocate/pool
datagram buffers. Sequence this *after* B1 (which changes the send loop anyway)
and treat it as its own design spike. **Seam:** `engine.rs:110-116`,
`lib.rs:262-294`, `audio.rs`.

### B8 — Codec choice (i24/f32, FLAC, Opus) · M · rs · 🟡 (after B4)
Native codecs beyond PCM i16. The wire already validates codec fields; native
mode chooses from the `tcap` intersection before sending (fixing v1's silent
mis-decode). Sequence after B4; each codec is an additive `tcap` bit + an
encode/decode path. **Seam:** codec dispatch in `tactus-wire/src/audio.rs`.

---

## Lane C — Better mesh

Cross-subnet reach and topology-aware routing, over an authenticated overlay, with
the trust tiers the owner asked for. This lane introduces the **`tactus-mesh`
workspace** (separate from link-wire-rs — see [§Substrates](#substrates) for the
license reason).

### C1 — `tactus-mesh` workspace scaffold + `mshr` wrap · M · mesh · 🟢
New workspace `tactus-mesh` (MIT, own `deny.toml`) with a thin adapter over
`mshr`: construct the endpoint with a **tactus-owned identity dir** (call
`Keypair::load_or_create_at`, *not* the yah-branded default at
`mshr .../keypair.rs:88-91`), one control ALPN, optional relay. This is the
overlay-gateway substrate. **Seam:** `mshr .../endpoint.rs` builder;
`Keypair::load_or_create_at` at `keypair.rs:48`.

### C2 — Overlay gateway impl (behind A6's trait) · L · rs+mesh · 🟢 (after A6, C1)
Implement the A6 gateway trait over the `mshr` overlay: a virtual gateway that
`connect`s/gossips instead of multicasting, feeds received datagrams into the
same `dispatch(...)` funnel, and competes in `best_path` once it has RTT samples.
The proposal's §8.1 framing ("the overlay is just another gateway") becomes
literally true. **Seam:** A6 trait; `mshr` `Connection` re-export
(`lib.rs:29`); inbound funnel `runtime.rs:72-80`.

### C3 — Signed identity binding: Link NodeId ↔ mesh Ed25519 · M · rs+mesh+spec · 🟡 (D6)
The spoofable 8-byte Link/v1 NodeId gets a signed binding to the peer's stable
mesh Ed25519 identity, gossiped via `tcap`/overlay. This is what lets you *always
know who's who* even in promiscuous jam mode, so lockdown is a policy flip rather
than a re-handshake. Needs a spec record shape (part of D6 gossip encoding).
**Seam:** `tcap` TLV (B3) for the binding record; verification in `tactus-mesh`.

### C4 — Trust tiers: jam / adhoc / stage · L · mesh · 🔴 (D7 — the owner's call)
The admission-policy state machine, riding `mshr`'s `Acceptor` hook
(`mshr .../endpoint.rs:95-98`, consulted post-TLS-handshake / pre-application-bytes
at `:366-373`, with the authenticated NodeId; stateful + `Arc`-shared, so
runtime tier switching works via `ArcSwap`):
- **jam** — no acceptor registered (accept-all), mDNS on, optionally LAN-only
  (no relay). Zero code beyond defaults; identity still collected + displayed.
- **adhoc** — TOFU: stateful acceptor admits first-seen NodeIds and pins them for
  the session; deny on key change. ~50 LOC.
- **stage** — pinned-roster acceptor (deny unless NodeId ∈ roster); roster built
  by a pairing ceremony (C5); non-roster peers ignored at accept and their
  channel requests refused.
Mid-session **lockdown** = freeze the roster to current members, carried on the
proposal's monotonic **policy epoch** (§8.3) so trust mode rides the same gossip
as the routing objective. **Placement is deliberate:** policy lives *here*, in
the MIT overlay crate — never in `ipauro` (GPL), or stage mode becomes
GPL-dependent. **Seam:** `mshr` `Acceptor` at `endpoint.rs:95-98`;
`PeerSource` bridge at `discovery.rs:69-73`.

### C5 — Stage pairing ceremony (fork-minimal from cheers lan-pair) · M · mesh · 🟢 (after C1)
Re-implement (do **not** depend on) the ~300-line `cheers` lan-pair pattern:
offer/confirm/accept over one authenticated bidi stream + a `ConfirmationStrategy`
trait whose three variants map exactly to the tiers — `AutoTrust` (jam),
`SixDigitCode` (adhoc/stage code entry), `DisplayCode` (stage visual compare).
Steal the load-bearing safety idea: bind trust to the *authenticated*
`remote_id`, never the self-reported NodeId, and hard-reject a mismatch
(`cheers .../lan_pair/accepter.rs:103-113`). Drop the account/ownership payload —
a tactus roster has no "authed to whom." **Seam:** pattern at
`cheers/crates/cheers/src/lan_pair/{mod,offerer,accepter,confirm}.rs`.

### C6 — Topology gossip records + deterministic route recompute · L · mesh+spec · 🔴 (D6)
The "shortest path" substrate (proposal open Q6). Pin four signed,
origin-sequenced gossip record types — `PeerRecord` (identities, transports,
`tcap`), `LinkRecord` (a→b RTT/jitter/loss/bandwidth), `DemandRecord` (flow needs:
codec, latency target, priority), `PolicyRecord` (epoch, mode, roster hash,
objective weights). **The spec pins the encoding; `ipauro` (GPL) owns the
algorithm.** Nodes deterministically recompute routes from converged inputs
(link-state style), damped by Link's own "strictly-better" rule (A4). Gossip runs
SWIM-style over the control ALPN — `mshr` has no gossip primitive, so tactus
brings its own (the proposal assumes this). **Seam:** encoding in `tactus-mesh` +
spec §; `PeerSource` feedback at `mshr .../discovery.rs:216-244`.

### C7 — VERIFY: iroh 1.0-rc unreliable datagrams + 0-RTT · S · mesh · 🔴 (online check, do first)
**Highest-priority unknown, and it gates the §8.2 media option.** iroh 1.0-rc.0
swapped quinn for n0's `noq` stack; whether its `Connection` still exposes RFC
9221 unreliable datagrams (and 0-RTT) could not be confirmed offline (no iroh
source on the review machine). Confirm online **before** committing to
QUIC-unreliable-datagram media on shared links. If present, it passes through
`mshr`'s `Connection` re-export unchanged. Note the iroh **relay** path is
QUIC-over-TLS-WebSocket (TCP, head-of-line blocking) → control/gossip only until
holepunch, **never** media (the proposal already forbids relayed media).

---

## Lane D — Spec process & hygiene

Lives in link-wire-spec; full detail in that repo's
[`REVIEW-2026-07-spec-backlog.md`](../../link-wire-spec/REVIEW-2026-07-spec-backlog.md).
Summary of what blocks the native chapters:

- **D1 — Tag releases (S).** No git tags exist despite CHANGELOG v0.1.0–0.4.3;
  the example candidate CI pins `SPEC_REF: v0.1.0`, so a consumer copying it
  cannot clone. Quick, load-bearing.
- **D2 — Gateway-trait design note (S, 🔴 decision).** Pin the endpoint-repr
  choice for A6 (synthetic `SocketAddr` vs abstracted runtime endpoint).
- **D3 — `tcap` fourcc + TLV registry (M).** Assign the real fourcc + TLV type
  numbers; add a registry section. Unblocks B3.
- **D4 — Normative `04-native-audio` chapter (L).** Native message type +
  grammar, codec negotiation, FEC framing, multicast join, clock-domain
  stamping, per-channel upgrade handshake, default max-chunk-frames. Unblocks
  B4/B6/B8.
- **D5 — Capture `discovery-ipv6.pcap` (M).** The one open `[B]→[W]` evidence
  upgrade; pairs with A2.
- **D6 — Topology/`tcap` gossip encoding (L, 🔴).** The spec-vs-ipauro boundary
  (proposal open Q6). Unblocks C3/C6.
- **D7 — Security Considerations chapter + evidence-model rebase (M, 🔴).** State
  the "trusted local segment" assumption and enumerate the LAN attack surface
  (below). Separately: the `[W]`/`[B]`/`[N]` evidence model **and** the
  conformance harness are *reference-anchored* — they mean "vs Ableton." Native
  chapters have no upstream reference, so upstream-watch, the canary, and the
  `[B]` class give **zero** drift detection for native content. Rebase the
  evidence model (own golden captures as `[W]`; a design-rationale class) before
  native chapters land — a structural finding, not a nit.

### Security posture (context for D7)

The protocol is "trusted local segment" **by design** — this is correct for jam,
not a bug to fix. Documenting it is the actionable. NodeId/SessionId are 8 random
bytes with no host binding; there is no crypto in the wire path. Inherent
LAN-gossip surfaces to name in D7:

- **Forged announcements** redirect a victim's measurement/audio traffic to
  attacker-chosen endpoints (endpoint learning trusts advertised entries).
- **Ghost-time election hijack** — the responder is stateless and answers any
  ping with its own ghost time, so a peer can present an arbitrarily large ghost
  time, win the join election (`02-sync.md` §7.2), and pull the session's
  timeline/tempo.
- **Channel-request amplification** — a spoofed request makes a sink stream
  ~768 kbit/s/channel until ttl; many spoofed requesters = sustained-egress DoS
  on the sink's segment (compounded by the open-loop §5.8 self-starvation).
- **ByeBye griefing** and **measurement reflection** — both unauthenticated.

The mesh overlay's authenticated QUIC is where identity/auth actually enters —
**above** the wire crate, never in the v1/native datagram path. That is the whole
point of the identity-constant / auth-tiered split.

---

## Sequencing

The dependency spine, longest pole first:

```
        A6 (gateway trait) ──┬─→ A1 (iface enum) ──→ A2 (ipv6) ──→ D5 (v6 vector)
                             └─→ C2 (overlay gw) ──→ C6 (topology routing)
                                     ↑
   C7 (VERIFY iroh dgrams) ─→ C1 (mshr wrap) ──────┘
                                     └─→ C5 (pairing) ─→ C4 (trust tiers)  [D7 security]
                                     └─→ C3 (id binding) [D6 gossip enc]

   B2 (non_exhaustive codec) ─→ B3 (tcap) ─→ B4 (native type) ─→ B6 (FEC)
        [D3 registry]              ↑             [D4 grammar]      ↑
                                   └─ B1 (packing, independent)    B5 (loss meas)
```

**Do first, in parallel, cheap and unblocking:** A3 (size caps), A4 (hysteresis —
also a mesh dependency), A5 (flush), B1 (packing), B2 (codec forward-compat),
B5 (loss measurement), C7 (the iroh verification), D1 (tag releases).

**Then the two prerequisites:** A6 (gateway trait) and the D3/D4 spec pins.

**Then the features layer on:** A1/A2, B3/B4, C1→C5→C4, C2→C6, B6/B8, B7.

Nothing in Lane C should start coding before **C7** resolves the datagram
question and **A6** lands the trait.

---

## Substrates

| Candidate | Verdict | Why | License |
|---|---|---|---|
| **mshr** | **Wrap** | Ed25519 identity (= iroh EndpointId), ALPN-multiplexed QUIC, and — the key seam — an `Acceptor` hook consulted post-handshake/pre-application-bytes with the authenticated NodeId (`endpoint.rs:95-98,:366-373`), stateful+`Arc`-shared for runtime tier switching; `PeerSource` trait to feed gossip→addressing. Wrap (not raw) to isolate: yah-branded identity default, iroh 1.0-rc churn, non-optional relay-server dep bloat, thin acceptor context. | MIT OR Apache-2.0 (compatible) **but** its tree pulls MPL-2.0/Unlicense/OpenSSL → fails link-wire-rs `deny.toml` → **`tactus-mesh` must be a separate workspace.** |
| **cheers** (crate) | **Skip** | Account/PASETO/JWKS/ownership-ledger machinery — solves "bind devices to a user account," not "peers admit peers." No account authority exists in a tactus mesh. | MIT OR Apache-2.0; moot. |
| **cheers lan-pair** (pattern) | **Fork-minimal** | Steal the ~300-line offer/confirm/accept + `ConfirmationStrategy` (AutoTrust/SixDigitCode/DisplayCode = jam/adhoc/stage) + authenticated-vs-claimed NodeId rejection (`accepter.rs:103-113`). Drop user/device/ownership payload. Depending on the crate would drag account vocabulary into the wrong domain. | Copy with attribution (permissive). |
| **iroh** | **Reach via mshr** | Everything needed beyond `mshr`'s surface is the re-exported `Connection` + the sanctioned `Endpoint::inner()` escape hatch. A direct pin duplicates the 1.0-rc churn `mshr` exists to absorb. **Open (C7):** confirm 1.0-rc.0 still exposes RFC 9221 datagrams + 0-RTT. | Dual MIT/Apache (ecosystem norm). |

**Layering (all sources agree):** `mshr` = mechanism only · `tactus-mesh` (MIT,
new workspace) = policy (tiers, roster, pairing, gossip bridge) · `ipauro` (GPL)
= routing algorithms only. Keeping admission policy out of `ipauro` is what keeps
stage mode GPL-free.

---

## Trust model

The owner's framing, made concrete: **identity is constant, authentication is
tiered.**

- Every peer always carries a stable **Ed25519 mesh identity**, and the overlay
  control plane is **always** encrypted+authenticated QUIC (free from `mshr`).
  What varies between modes is **admission policy**, not crypto.
- The spoofable 8-byte **Link/v1 NodeId** gets a **signed binding** to the mesh
  identity, gossiped via `tcap`/overlay (C3). So even in promiscuous jam mode you
  *know who's who* — which makes "lock the room down" a **policy-epoch flip**, not
  a re-handshake.
- **jam** (open) → accept-all, LAN-friendly, identity still displayed.
  **adhoc** (TOFU) → admit first-seen, pin for the session.
  **stage** (locked) → pinned roster via pairing ceremony; non-roster ignored.
- **Mid-session lockdown** = freeze roster to current members, carried on the
  monotonic policy epoch (§8.3) so the trust mode propagates on the same gossip
  as the routing objective.

Promiscuity is a *feature* of jam, bounded by LAN scope and by the fact that the
overlay still authenticates identity — you can always see, name, and later admit
a peer. Lockdown loses nothing and requires no new handshake.

---

## Decisions that need a human

Flagged 🔴/🟡 above; collected here so they don't get lost:

- **D2 / A6 — endpoint representation.** For the overlay gateway: map overlay
  peers into a synthetic `SocketAddr` space (smaller change, keeps the runtime
  endpoint type concrete) vs abstract the runtime endpoint type (cleaner, larger
  refactor). *Recommendation: synthetic `SocketAddr` for v1, revisit if it leaks.*
- **D4 — native message-type number + grammar.** Proposal suggests type 16;
  needs the payload grammar and FEC framing pinned before B4/B6.
- **D6 — spec vs ipauro boundary.** Exactly which topology/`tcap` fields the spec
  fixes (for interop) vs what `ipauro` owns (the routing math). The one open
  question the CHANGELOG explicitly re-opened.
- **D7 — trust-tier UX + security-doc scope.** The jam/adhoc/stage tier
  definitions and the pairing gestures are a product call; the security chapter
  states the trust assumption. *This is the owner's call — C4 waits on it.*
- **C7 — the iroh datagram verification.** Cheap, but it changes the §8.2 media
  design; do it before any Lane C coding.

---

## Appendix — yah agent-orchestration review (original brief, deferred)

The session's original brief was a holistic review of the **yah** agent-
orchestration layer (camp hub, MCP dispatch, bidirectional agent↔agent comms,
multiplayer human chat). That analysis is complete and summarized here for
continuity; it is orthogonal to link-wire and belongs in the yah repo if pursued.

**Headline findings:**
- **No push channel anywhere.** Every parent↔child agent interaction is either a
  blocking daemon RPC or 300 ms file-polling over session JSONL — there is no
  streaming and no way to wake an agent mid-turn. "Bidirectional" today means
  poll-or-block, by explicit design ("the parent's turn model is sacred," W133).
- **No human identity.** Forms answers, approval choices, and injected messages
  are all anonymous; `PeerSender` provenance exists for *agents* but not humans.
  Approval/forms plumbing (`PendingApprovals`, `FormApprovalMappings`) is
  per-desktop-process in-memory → two clients = split-brain. Multiplayer human
  chat's first prerequisite (per-session human attribution) is designed on paper
  in W159 Phase 3 / R428 (PASETO `sub`=human + `act`=agent) but the
  conversation/transport substrate and camp↔camp routing are explicitly deferred.
- **Fragmented dispatch.** At least five coordination disciplines coexist (runner
  `subagent_spawn`, PVd `camp.assist`, gnomes bite-queue, sage poll-claim loops,
  the board as the "settled" channel), plus two session registries with
  asymmetric capabilities (runner-tracked vs subprocess). Inter-agent features
  silently degrade by engine family.

If yah orchestration is picked back up, the natural first tickets are: a
daemon-owned event/subscription bus (kills the poll-or-block pattern), first-class
human identity on the answer queue (unblocks multiplayer), and a single session
registry. Full notes available on request.
