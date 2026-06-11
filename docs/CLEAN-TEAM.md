# Clean-team charter

This file is the standing prompt under which clean-room implementation
sessions in this repository operate, committed so the working rules are part
of the repository's provenance record. It supplements — and never overrides —
[PROVENANCE.md](../PROVENANCE.md), which is binding.

## The brief

> You are the clean-room implementer. Read PROVENANCE.md first — it is
> binding. Your only protocol inputs are: the released specification of
> [structuresound/link-wire-spec](https://github.com/structuresound/link-wire-spec)
> including its `vectors/` packet captures; F. Goltz, *"Ableton Link"*
> (LAC 2018); and Ableton's public help pages. You must not clone, fetch,
> read, or search for: `Ableton/link`, `Ableton/LinkKit`, `ableton-link-rs`,
> `rusty_link`, or any other implementation of this protocol. If your
> training data contains memories of such code, do not draw on them — derive
> everything from the spec. When the spec is ambiguous, incomplete, or
> appears wrong, do not investigate the reference: open an issue on
> link-wire-spec stating the ambiguity and what observation would settle it,
> and work on something else until it's answered. That issue channel is the
> only bridge across the firewall.

## Milestones (each gated by tests)

- **M0** — Cargo workspace, MIT license, CI. Propose a crate name that is
  not Link-branded and confirm it before publishing anything.
- **M1** — Wire codec: the serialization primitives from spec chapter 00 and
  encode/decode for every message in chapters 01–03. Acceptance:
  byte-for-byte round-trip of every packet in `vectors/`.
- **M2** — Discovery + sync peer (chapters 01–02): join a session, converge
  tempo, track beat phase, handle peer churn and start/stop.
- **M3** — LinkAudio v1 (chapter 03): channel announce/request lifecycle,
  PCM i16 sink and source, beat-time-aligned scheduling.
- **M4** — Wire up the conformance harness under `conformance/` (contributed
  from the spec side). Treat its output strictly as observations; never open
  files under its reference cache. Acceptance: interop scenarios pass against
  reference peers on loopback.

Every PR description includes: *"I have complied with PROVENANCE.md; this
contribution is derived only from permitted inputs."*

## The clean-room execution environment

Clean-room coding sessions run in a managed environment named **`clean
room`** whose network allowlist is, exactly:

```
crates.io
index.crates.io
static.crates.io
static.rust-lang.org
lac.linuxaudio.org      # the Goltz LAC 2018 paper (permitted input)
help.ableton.com        # Ableton's public Link FAQ (permitted input)
```

The default package-manager allowlist (which includes `github.com`) is
disabled. Deliberately absent:

- **`github.com`** — blocks fetching `Ableton/link` and disables cargo git
  dependencies entirely (a feature: forces registry-only deps). The two
  project repositories are attached to sessions through the platform's
  internal git proxy, which is how a clean session reads the spec release
  and files ambiguity issues without `github.com` being reachable.
- **`docs.rs`** — its "source" tab renders the full source of any published
  crate, including GPL ones. Build docs locally with `cargo doc` instead.

### The crates.io gap, closed at the cargo layer

GPL implementations of this protocol are published on crates.io
(`ableton-link-rs`, `rusty_link`, `abl_link`), so `static.crates.io` could
serve their source despite the domain policy. That path is closed by
[`deny.toml`](../deny.toml): `cargo deny check bans` runs in CI and fails
the build if any of those crates (or `ableton-link`) ever enters the
dependency graph. Defense in depth: the network policy stops `github.com`,
cargo-deny stops the dependency path, this charter stops deliberate
fetching, and the session transcript proves none of it happened.

The spec-side ("dirty room") environment is the opposite: it must reach
`github.com` to read upstream. Its rules are editorial (no copied
expression) and are enforced by the spec repo's PROVENANCE.md and review,
not by network policy. Implementation sessions for this repository start
only from the `clean room` environment; the environment name in session
metadata is part of the provenance record.
