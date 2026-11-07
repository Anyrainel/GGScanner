# Capture upstream review — 2026-11-07

## Sources and result

| Boundary | Reviewed public upstream | Result |
| --- | --- | --- |
| GI and HSR dispatch tables | [StarDB Exporter](https://github.com/juliuskreutz/stardb-exporter/tree/6c4b4d31e3ffdf392616cdecc38ffe13603f85df), `keys/{gi,hsr}.json` | All decoded identifiers and 4096-byte values match; GI 10 keys, HSR 31 keys. |
| GI RSA keys, RNG, XOR, token exchange | [auto-artifactarium](https://github.com/konkers/auto-artifactarium/tree/7dafbf0af0bfece23e46382dd3f991d39a5436ac), `keys/`, `src/{crypto,cs_rand,unk_util,lib}.rs` | RSA 4/5 files match upstream Git blobs exactly. XOR, version lookup, RSA PKCS#1 v1.5 seed decoding, C# RNG candidate search and MT64 expansion are semantically unchanged. |
| HSR XOR, seed and transport | [Reliquary](https://github.com/IceDynamix/reliquary/tree/0b8c1d4f8eb5ae9722b5c3273e36a464b7188a38), `src/network/` | Version lookup, repeated-key XOR and MT64 key expansion match. Incoming dispatch-token seed inference replaces the command/tag selector; KCP state remains separate per conversation. |
| HSR inventory/Quest schemas | Same Reliquary revision, `src/network/command/proto/` | All seven bundled inventory record files have exactly matching Git blob IDs. Quest/response fields and the uint64 token seed were checked against the current schemas. |

The source fingerprints, complete watched-file lists, reviewed revisions and
LF-normalized local SHA-256 values are in
[`capture-upstreams.json`](capture-upstreams.json). Key bytes and account/session
data are not included in reports. StarDB is the application dispatch-key source;
Reliquary's older embedded key fallback is not used by the local HSR transport.

## Intentional local differences

- The original GI vendored base remains `4ba25fac64b88970143af6bc2a2ef51338e620d0`.
  The two later upstream commits only rotate command IDs and change
  `PacketWithItems.items` from outer field 5 to 2. Production
  `genshin/src/capture/monitor.rs` enumerates protobuf fields and validates
  records without those selectors. Those old, unused vendor selectors do not
  require changing working decryption or rejecting older packets.
- GI adds malformed-command/KCP bounds, key-length validation, diagnostic
  metadata, fallible timestamp parsing and handshake-readiness reset. These
  preserve the upstream cryptographic operations. See
  [`vendor/auto-artifactarium/PATCHES.md`](../vendor/auto-artifactarium/PATCHES.md).
- HSR omits raw packet/seed trace output, accepts only application-supplied
  dispatch keys, bounds malformed commands and infers a unique uint64 seed
  during incoming dispatch decryption. It does not depend on the upstream token
  command ID or seed tag. This inference requires a seed larger than uint32 and
  an unambiguous token shape; it is not a promise of arbitrary future layouts.
- Inventory outer-field discovery, public-ID validation and local Quest
  normalization are application behavior. Preserve these when reviewing an
  upstream schema update; do not replace them with command-ID gates.

## Weekly verification

Run from the GGScanner repository root:

```powershell
uv run --no-project python scripts/check_capture_keys.py
uv run --no-project python scripts/check_capture_upstreams.py
uv run --no-project python -m unittest discover -s scripts -p 'test_check_capture_*.py'
```

The first command compares changing dispatch tables with one freshly resolved
StarDB commit, retaining historical local-only keys. The second resolves each
upstream's current default branch, then compares reviewed file fingerprints at
one immutable commit per repository. An unchanged reviewed commit requires no
file downloads. New commits outside the watched files pass. A watched source,
RSA file or reviewed local decoder change requires semantic review. A network,
missing-file or malformed-metadata failure explicitly leaves consistency
unverified and exits nonzero. Neither checker edits files or updates baselines.

When review is required, read the actual upstream diff and current local callers.
Check new modules introduced by changes to the watched module declarations and
dependency manifests as well. Repair only material drift, preserve the intentional
differences above, run the affected capture tests, and update this record and the
fingerprint manifest together after review. Do not refresh fingerprints merely
to make a failing check pass.

For decoder changes, direct verification commands are:

```powershell
cargo test --locked -p genshin_scanner --features capture capture
cargo test --locked -p hsr_scanner --features capture
```

Redirect long Rust test output to `test-results/`, inspect it and remove it after
the run. Existing key checks in pre-push/CI remain unchanged. The weekly source
checker is a review alarm, not a new release gate.

This review passed both online checkers, all 11 Python checker tests, 52 Genshin
tests selected by the capture filter, and all 205 HSR tests with capture enabled.

This review establishes agreement with published references and deterministic
replays. It does not establish a fresh live login/capture on either game or
detect key/protocol changes the upstream projects have not published.
