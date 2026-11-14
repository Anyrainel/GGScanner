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

## Account identity extension — 2026-11-08

Reviewed Reliquary `938287c5cbba930ad68aa3f7010cf7ad953bc654`
(24.0.1). The previously watched upstream files are unchanged except for the
Cargo package version, from 24.0.0 to 24.0.1. Added `PlayerLoginScRsp.rs` and
`PlayerBasicInfo.rs` to the watched source set alongside the existing token type.

The local HSR identity reader accepts only incoming token/login commands under
the reviewed 4.6 dispatch-key identifier and exact success layouts. UID and
nickname stay attached to one KCP conversation. Existing retained raw frames
were replayed through the production decoder: exported UID and nickname matched
the same login, opaque item IDs were unique, and no transport errors occurred.
Private frames and exports remain outside source control. Synthetic tests cover
account switches, retired traffic, reconnects, unidentified-layout fallback,
normal/continuous exports and stable item identities across upgrades.

The local connection wrapper now retains the disconnect conversation ID rather
than invalidating whichever account happens to be selected. The Genshin vendor
wrapper clears KCP/key/seed/client-seed state on disconnect and rejects delayed
segments until an explicit new handshake request; the cryptographic operations
remain unchanged. Its synthetic encrypted-packet regression and full library
tests pass. These intentional wrapper changes and the new HSR identity module
have reviewed local fingerprints in `capture-upstreams.json`.

At the November 8 review, Genshin UID/nickname detection remained unverified for
official-server packets. Optional GOOD identity metadata was supported, while
unidentified captures used the website's selected-profile fallback. The
completion below supersedes that UID limitation. Neither review claims a fresh
live-game capture.

## Genshin login identity completion — 2026-11-12

Reviewed AstaPS
[`a49f51aa6f2787ffbc25cee46801b70dee860d23`](https://github.com/MeChen618/AstaPS/blob/a49f51aa6f2787ffbc25cee46801b70dee860d23/src/main/proto/GetPlayerTokenRsp.proto).
The current login response identifies the game UID at field 9, success return
code at 14, key ID at 440 and RSA server seed at 477. A retained November 9
official-server response matches these fields; field 477 decrypts to exactly
eight bytes with the bundled key. The public game UID differs from the
HoYoverse account identifier at field 5. Current retained PacketHead data has no
user ID; the legacy bundled header's `user_id` field is not used for identity.
The referenced login schema is now watched by the source drift checker.

Production command parsing now separates header and body, reads seed timing
only from the header and retains direction. The identity reader validates the
specific reviewed RSA field, success, unique UID and supported key ID. The
monitor clears identity and inventory together at login boundaries; snapshots
require both verified UID and baseline inventory. Both KCP directions share one
conversation and reject retired traffic, including stale disconnect controls.
Same-account reconnects create a new snapshot session. Retired conversation-ID
reuse requires a listener restart because fresh reuse cannot be distinguished
from delayed controls with the validated handshake information.

Synthetic fully framed login fixtures exercise production Ethernet/KCP/RSA
decoding, nonempty GOOD exports, outgoing-token rejection, account switches,
same-UID reconnects and invalid-identity withholding. A local-only replay also
fed the retained official response through the production decoder and matched
its game UID. The legacy dump retained header plus body without original
lengths; this replay reconstructed the boundary from the independently parsed
header. It verifies the real token body and RSA/UID decoder, and is not a fresh
raw-network capture. New command dumps retain complete framing for later
replays. Private login data remains outside source control.

Validation passed 14 vendor decoder tests, 335 Genshin tests, 74 website scanner
tests, 11 checker tests and the release executable build. Bundled keys and
watched Genshin upstream sources match. The combined source checker separately
flagged newer Reliquary (Star Rail) upstream changes; that unrelated review
baseline was left unchanged.

## Continuous inventory deletion review — 2026-11-12

The existing reducers already replaced equipment records under the same instance
ID. Continuous deletion support now consumes explicit server notifications,
retains ready empty inventories, and tests upgrade/consume/salvage sequences.

For Genshin, reviewed Teyvat.me Capture
[`05a3bb8b`](https://github.com/tawan475/teyvat.me-capture/tree/05a3bb8b0f8c02432c307ae43ba68277e2141206),
specifically `crates/protocol/src/lib.rs` and `unk_util.rs`. Its strict
StoreItemDelNotify matcher requires exactly one packed GUID list and a
store-type scalar (1 or 2). The local implementation additionally requires
incoming traffic, verified login identity, and known held instances. Looser
GUID discovery can mistake enhancement targets or strongbox outputs for deleted
items. The MIT attribution is retained in `THIRD_PARTY_NOTICES.md`.

For Star Rail, reviewed Reliquary's existing pinned 4.6 revision
[`938287c5`](https://github.com/IceDynamix/reliquary/tree/938287c5cbba930ad68aa3f7010cf7ad953bc654),
including `command_id.rs` and `PlayerSyncScNotify.rs`. Destructive deltas require
dispatch key identifier `2920436228` and incoming command `634`:
`del_equipment_list` is field 4 and `del_relic_list` field 15, with packed or
unpacked uint32 values. Unknown IDs do not delete held records. The exporter
retains cumulative account-scoped opaque deletion IDs for polling consumers;
the website removes exact matching captured instances before its normal partial
merge. Missing inventory alone remains insufficient evidence for deletion.

The current upstream Star Rail 4.7 schemas rotate command IDs and inner equipment
fields. The existing local inventory decoder and dispatch-key set remain 4.6;
this change does **not** establish 4.7 compatibility. Updating that protocol and
validating a fresh login remain separate work. The source drift alarm should
continue to report the unreviewed 4.7 changes rather than bless them by updating
all fingerprints.

Also compared the ZZZ reference
[`cec65746`](https://github.com/pingwinekZ/zzz_packet_capture_0delay/blob/cec65746da79b6d85f55752cb5487436a7408f48/rust/crates/zzz-scan/src/snapshot.rs).
It replaces equipment by UID and handles explicit `deletedEquips`; its broad
unknown-field deletion fallback and request-side dismantle deletion were not
adopted. Its empty-category-to-null export conversion was also not adopted:
loaded empty inventory must remain distinguishable from inventory not captured.

These findings and synthetic packet regressions establish implementation
behavior. No fresh live-game enhancement, strongbox or salvage session was run.

Validation passed 340 Genshin tests (4 ignored), 214 Star Rail tests, 21 Genshin
website continuous-import tests, the full GGStarRail check (1,263 app tests and
45 Worker tests), 11 capture-checker tests, Rust formatting and the Windows
GGScanner release build. All 35 reviewed local source fingerprints match.

## Required packet-capture identity — 2026-11-13

Both one-time and continuous packet capture now require a verified login UID
before reporting successful completion or writing a public export. Genshin's
one-time monitor uses the same UID gate as continuous capture; an unsupported
token layout reports an actionable error in either mode. Star Rail rejects
otherwise complete packets without verified identity, and its public export
builder independently rejects missing UID. Command-only diagnostic replays can
still inspect anonymous semantic data, but cannot write successful public
packet-capture exports. Existing OCR/export import compatibility is unchanged.

This tightens the November 8 anonymous-capture behavior; no continuous-capture
API has been publicly released that requires an anonymous legacy path. Automatic
website sync routes by UID to its matching account and routes UID-absent input
to Default. It never guesses identity from the currently selected profile.

The accepted UID protocols remain the reviewed Genshin 7.2 RSA-verified token
layout and HSR 4.6 dispatch revision `2920436228` documented above. No guessed
integer fields, new protocol layouts or additional live-game compatibility are
claimed. Synthetic full-packet tests cover both capture modes, UID-bearing
exports, account switches, stale identity clearing and missing-identity errors.
A fresh login with the rebuilt application remains the external live-game check.
Star Rail also surfaces the transport's incoming session-seed transition as a
login boundary when the UID layout is unsupported. This clears the previous
account even if the establishment frame was missed; retired traffic cannot
restore its identity. No UID is inferred from that seed transition.

Validation passed 343 Genshin tests (4 ignored), 218 Star Rail tests, 21 app
shared-game integration tests, 18 GUI feedback tests and 11 capture-checker
tests. The production-feature Windows scanner binaries pass `cargo check`;
changed Rust files pass formatting and all 35 reviewed local fingerprints match.

## Current Star Rail 4.7 protocol and reference — 2026-11-14

Reviewed Reliquary v25.0.0 commit
[`ba9319be47e406ef5bf52c6b7b04b859c65a8026`](https://github.com/IceDynamix/reliquary/tree/ba9319be47e406ef5bf52c6b7b04b859c65a8026),
whose change updates the protocol to 4.7. This supersedes the earlier 4.6-only
identity/inventory limitation. The seven local generated record types match the
primary files exactly; `AvatarPathData.ExRankList` remains separate from normal
Eidolon rank. Initial roster command 319 uses path field 7, base field 8 and
`is_get_all` field 15; bag command 519 uses equipment field 9 and relic field 11.

The token is command 71: seed field 9, UID field 10, success return code 11.
Login is command 19: basic info field 10 and success return code 4. Basic info's
nickname field 1 and level field 2 are unchanged. These layouts require the
reviewed 4.7 dispatch selector `2766937619`; historical keys cannot authorize
4.7 identity or deletions. The new 4096-byte public key comes from this revision's
[`src/network/crypto.rs`](https://github.com/IceDynamix/reliquary/blob/ba9319be47e406ef5bf52c6b7b04b859c65a8026/src/network/crypto.rs),
with decoded SHA-256
`4f25f94bf7e5ee986602a08cc3e06806aa933312022df415e3add67d259fd03c`.
All 31 older bundled keys are retained unchanged. StarDB's current table still
has only those 31 entries, so key freshness now checks both immutable StarDB and
Reliquary revisions and rejects disagreements or unrecognized Rust key arms.

Continuous sync command 674 uses relic records at 2, equipment at 3, relic
deletions at 10 and equipment deletions at 11. Explicit packed/unpacked positive
instance IDs remain required. The strict message-field allowlist is copied from
the reviewed notification; absent inventory alone still does not imply deletion.
Quest response 919 uses success field 4 and record field 9; record ID/status/
progress/finish-time/counter tags are 11/3/8/10/7. Finish times remain excluded.

The existing embedded-reference generator was run against HoyoData's verified
2.1.0 bundle from producer `0e63fc8fce3e00d1dadef9c695e121e89644aeca`, source
`dfdfe3337bcfa14f000074d2dae7058118193ea6` (4.7). The fallback includes all
99 characters, 172 Light Cones and 790 Relic variants in the current shared
reference, plus 1,993 achievement IDs. Packet-affix tables are semantically
unchanged and carry the same refreshed source provenance.

Synthetic fully framed regressions use the actual bundled 4.7 dispatch key and
manually authored primary wire tags, including current character 1511, Light
Cone 23065 and Planar Ornament 63295. They cover one-time capture with Quests,
continuous capture, account identity and explicit deletions. No fresh live-game
login is claimed by these tests. Publish the updated website shared reference
before releasing the scanner; online cache/refresh behavior remains unchanged,
while the regenerated embedded reference supports cold offline startup.

Validation passed 219 Star Rail tests, 21 shared-game app integration tests,
18 Python capture-checker tests, production-feature binary `cargo check`, the
embedded generator's byte-for-byte `--check`, live verification of both key
sources, Rust formatting and all 35 reviewed local source fingerprints. Embedded
and current website reference IDs match exactly across all three inventory families.
