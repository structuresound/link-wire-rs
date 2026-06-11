# Provenance rules — implementation side ("clean room")

This repository claims clean-room provenance: its code is written from a
published protocol specification, not from the GPL reference implementation.
That claim is only as good as the discipline below.

## Permitted inputs

Code in this repository may be derived ONLY from:

1. Released versions of
   [link-wire-spec](https://github.com/structuresound/link-wire-spec)
   (specification text and CC0 test vectors).
2. Output of the conformance harness, phrased as observations
   ("tempo diverges after a peer rejoins"), never as reference-source diffs.
3. Public non-GPL documentation: F. Goltz, *"Ableton Link — A technology to
   synchronize music software"* (LAC 2018); Ableton's public help pages.
4. Generic, protocol-unrelated dependencies and literature.

## Forbidden inputs

- The `Ableton/link` and `Ableton/LinkKit` repositories: source, headers,
  diffs, commit messages, issues — in any form, including quoted in chat.
- GPL implementations of the protocol: `ableton-link-rs` (GPLv3),
  `rusty_link` (binds GPL code).
- Decompilation or disassembly of shipped Ableton products.
- For AI-authored code: recalling the reference implementation from training
  data. Authoring sessions are instructed to work from the spec alone; the
  similarity audit below is the backstop.

## Process rules

- **Sessions are one-way.** A person or agent session that has read a
  forbidden input is permanently "dirty" and must not author implementation
  code here. Dirty contributors may file conformance *observations* and
  review for similarity, nothing more.
- **The conformance harness uses GPL software, it does not create a
  derivative.** Building and running upstream reference binaries
  (`LinkHut`, `link_audio_hut`) as interop test peers in CI is use, which
  GPLv2 does not restrict. Reference source stays in CI caches, is never
  vendored into this repository, and harness code must not incorporate it.
- **Contributor certification.** Every PR certifies: "I have complied with
  PROVENANCE.md; this contribution is derived only from permitted inputs."
- **Similarity audit.** Before each release, new code is reviewed against
  the reference implementation for non-literal similarity by a dirty-side
  reviewer. Audit notes (verdict only, no source excerpts) are recorded in
  the release notes.
- **Spec gaps go upstream.** If the spec is ambiguous or wrong, the fix is a
  spec issue/release on link-wire-spec — never "someone checks the C++ and
  tells the implementer." The spec is the only bridge across the firewall.

## License

MIT. See [LICENSE](LICENSE).
