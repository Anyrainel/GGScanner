# Star Rail capture

Select **Star Rail → Capture**, start capture as administrator, then log in
from the game's title screen. Characters, Light Cones, and Relics (including
Planar Ornaments), and Achievements & objectives each have a checkbox, matching the Genshin
export settings. All four default to selected; selections persist independently
of OCR settings. Unchecked categories are excluded from the export and completion
requirements. A verified login UID is required for every packet capture, including
one-time and continuous capture. Capture stops and saves one export file after
the identity and requested data arrive. The default destination is beside the
executable; existing saved output destinations are retained. The result shows
the actual file path.

All HSR inventory flows download the shared reference from
`https://hsr.ggartifact.com/good/hsr_scanner_data.json` (formatVersion 2), cached
at `data/hsr/hsr_scanner_data.json`. It contains OCR names, inventory definitions,
affix values and packet/form mappings, with no achievement data. Scanner, capture
and manager reuse this one cache. The old combined filename and the briefly
separated OCR/capture caches are deleted on load, along with empty obsolete directories.

Capture exports the complete Quest response when Achievements & objectives is
selected, retaining IDs, statuses, progress and counters. It requires no
achievement-ID download. GGStarRail classifies achievements with its own catalog
and displays the remaining records in the Objectives archive. The obsolete
`hsr_achievement_ids.json` cache and hosted file are removed. The full achievement
OCR mapping remains a separate product.

The base download uses the two-hour cache policy, with its own refresh button
and download age. Failed or malformed replacements preserve the previous cache and report
the error. Normal loads can use a last valid cache; a cold offline load can use
the bundled reference.

HoyoData's `uv run hoyodata ggstarrail` generates the base file from the same
source and publishes them into GGStarRail's `public/good/` directory and its local
archive. The website build checks revision and field coverage against its catalog.
Publishing the files updates clients without rebuilding GGScanner. Protocol code
changes can still require an application update.

Application config v1 migrates to v2 by dropping the obsolete developer
reference path while preserving scan preferences, output path, and navigation.


## Advanced settings and export files

Like Genshin, **Advanced** contains **Dump decrypted packets** and **Only keep
latest export**. Both default off and persist independently of Genshin and OCR.
Packet diagnostics go beside the executable in `debug_capture/hsr/hsr_<session>/`,
with numbered `.bin` protobuf bodies. Each capture gets its own folder. This is
an explicit diagnostic dump, separate from sanitized exports; it is not a PCAP
or ETL transport recording. A write failure is surfaced with its full path.

Replay a raw dump through the live transport and current hosted references with
`cargo run -p hsr_scanner --features capture --example analyze_dump -- <session> --raw`.
The tool reports distinct transport errors and exports only a complete snapshot.

Each completed capture writes one file: `star_rail_capture_<timestamp>.json`.
That file is HSR-Scanner format v4 plus GGScanner extensions (complete Quests,
Trailblazer gender and current path). See [`HSR_EXPORT.md`](HSR_EXPORT.md).
Screenshot scans write `star_rail_scan_<timestamp>.json` instead, so the two
flows cannot overwrite each other. Unchecked categories are omitted rather
than reported as empty-complete. Existing `star_rail_capture_`, legacy
`star_rail_export_`, `star_rail_fribbels_`, and `star_rail_achievements_`
files are removed only after the new capture file has been written and
synced. Cleanup never deletes `star_rail_scan_` files, `genshin_export_`
files, packet dumps, directories, or symlinks.

Fribbels currently allowlists `HSR-Scanner` as a source string. The interchange
file uses that required v4 compatibility discriminator and its compatible
build identifier; a separate `generator` object keeps the historical GOODScanner name and
its capture revision. It does not claim to have been captured by Kel-Z's tool.
Native server item IDs are never exported. Verified capture accounts receive
stable opaque item IDs; OCR exports retain scan-order IDs. UID and
optional nickname come from the reviewed incoming login; see the identity
section below. The primary format references are
[Fribbels' parser](https://github.com/fribbels/hsr-optimizer/blob/main/src/lib/importer/kelzFormatParser.tsx)
and [import configuration](https://github.com/fribbels/hsr-optimizer/blob/main/src/lib/importer/importConfig.ts).

Validation replayed generated inventory through the current Fribbels v4 parser
with real public item/affix tables (UI notifications and post-parse augmentation
isolated), and through GGStarRail's v4 schema. Live game capture and uploads to
every third-party site are not claimed by these tests.

## Protocol approach

Research checked the public heads of
[Reliquary](https://github.com/IceDynamix/reliquary/tree/d5cf3b7e7e66470d2d8efff6676aa18762b21d3b)
(v23.0.0 / HSR 4.5, 2026-08-28) and
[Reliquary Archiver](https://github.com/IceDynamix/reliquary-archiver/tree/cb109f17a4a15b7604cfe9d078a8735e7735cd25)
(v0.18.0, 2026-08-29). English and Chinese searches also identified
[StarDB Exporter](https://github.com/juliuskreutz/stardb-exporter), its
[Chinese importer](https://stardb.gg/zh-tw/import), and
[HSR Scanner](https://github.com/kel-z/HSR-Scanner). The first two Reliquary
projects provide the maintained packet-capture reference; HSR Scanner uses OCR.
See the [license/provenance notice](../experimental/hsr/THIRD_PARTY_NOTICES.md).

The old native path could export only achievements and stopped as soon as an
achievement response was recognized. Its transport had one shared pair of KCP
decoders, unlike current Reliquary's independent conversations. Its achievement
matcher also rejected the packed counters allowed by current Quest records.
These are code-level defects; without the failing live session they cannot be
claimed as the confirmed cause of every login that waits indefinitely.

The replacement uses:

- pktmon on HSR UDP ports 23301/23302, with cancelable stream ownership.
- Separate KCP and decryption state per conversation. Login seed inference is
  limited to incoming dispatch-encrypted commands, without a token command ID.
- Bounded traversal of valid protobuf wrappers. Every repeated field is tested
  against inventory record types and public reference IDs. Container field
  numbers and byte positions are not assumed.
- Base Avatar progression joined to AvatarPathData using public multipath
  definitions, including Trailblazer/March variants. The complete-roster flag
  distinguishes a full avatar response from a normal avatar update.
- Equipment and Relic normalization, retaining equipped character, lock and
  discard states; affix IDs/roll counts become the existing semantic stat units.
- Separate Quest readiness. All records in a candidate group must parse;
  an invalid member cannot silently disappear from an authoritative snapshot.
- A 120-second deadline with readable errors distinguishing absent traffic,
  unsuccessful decryption, and missing categories. Full errors include packet,
  decrypted-command, and category counts plus the last transport failure.

There are no executable offsets, process-memory reads, injected code, helper
executables, or packet writes. This follows the Genshin capture architecture's
distinction between unknown outer protobuf containers and validated inner types.

## Compatibility limits

A local login-frame replay confirmed that `GetQuestDataScRsp` carries achievement
and non-achievement Quest records in the same repeated field 10. Quest ID is
field 12, status is field 1, completion time is field 3 and progress is field 14
in that inspected layout. Both categories share record shapes; those fields
do not include a category discriminator. GGScanner preserves the complete list;
classification happens in GGStarRail using AchievementData and QuestData.
The inner Quest schema is the reviewed HSR 4.6 layout, including packed or
unpacked counters in field 10. Incremental Quest notifications are not accepted
as complete snapshots.

For offline investigation, `analyze_dump <session> --raw --dump-dir <diagnostics>
--output-dir <exports>` replays raw frames through the production decoder and
retains the unfiltered decrypted messages in the diagnostic directory. Separate
output directories keep the original capture session intact. This diagnostic
material must remain local and must not be committed as a test fixture.

The inner inventory schemas are the reviewed HSR 4.6 protobuf types from
Reliquary revision `0b8c1d4f8eb5ae9722b5c3273e36a464b7188a38`. Command
and outer-field rotation are handled, but arbitrary inner-field or encryption
changes can still require an update. Scalar fields with identical shapes
cannot always be inferred unambiguously from values alone.

The inventory matcher requires nonempty Light Cone and Relic groups together;
missing groups do not prove an empty inventory. Such sessions time out instead
of exporting an authoritative empty collection. Capture writes HSR-Scanner v4;
see [`HSR_EXPORT.md`](HSR_EXPORT.md). OCR and manager flows still use the
GGStarRail schema.

## Verification

Automated coverage includes interleaved encrypted KCP conversations, inferred
seed tag changes, reordering/duplicates/truncation, rotated container tags,
nested wrappers, packed Quest counters, incomplete/ambiguous data, multipath
normalization, timeout/cancellation, and off-screen mixed-export completion.
The native Capture screen was inspected in English and Chinese.

A fresh login against the rebuilt binary remains the final live check. Replay
fixtures are synthetic, not captured player traffic. No live-success claim is
made from those fixtures.


## Captured account identity

The reviewed HSR 4.6 incoming login layout exposes UID in
`PlayerGetTokenScRsp` (command 16, field 15), and nickname in
`PlayerLoginScRsp` (command 25, field 4 `PlayerBasicInfo`, then field 1).
The decoder pins identity parsing to dispatch-key identifier `2920436228` and
the schemas in Reliquary commit `938287c5cbba930ad68aa3f7010cf7ad953bc654`:
[token](https://github.com/IceDynamix/reliquary/blob/938287c5cbba930ad68aa3f7010cf7ad953bc654/src/network/command/proto/PlayerGetTokenScRsp.rs),
[login](https://github.com/IceDynamix/reliquary/blob/938287c5cbba930ad68aa3f7010cf7ad953bc654/src/network/command/proto/PlayerLoginScRsp.rs), and
[basic info](https://github.com/IceDynamix/reliquary/blob/938287c5cbba930ad68aa3f7010cf7ad953bc654/src/network/command/proto/PlayerBasicInfo.rs).
Unknown revisions, failed responses, outgoing requests and arbitrary matching
integers/strings never establish identity. Existing local raw login frames
confirmed populated fields in this layout; private frames are not test fixtures.

Identity is retained per KCP conversation. A verified new login clears the
previous inventory and identity before selecting that conversation; delayed
commands and disconnects from a retired conversation cannot update or invalidate
the new account. A fresh established conversation, verified token, or selected
conversation disconnect invalidates the baseline and advances its generation.
An incoming session-seed transition also invalidates the previous account when
its identity schema is unsupported and the transport handshake was missed.
An unknown identity schema cannot complete either one-time or continuous
capture. Once the requested data arrives without verified identity, capture
reports `HSR-CAPTURE-UID` with an update/re-login instruction; it never silently
exports anonymous data or inherits the previous account's UID. Incomplete
sessions continue waiting, with UID-specific timeout diagnostics when needed.
Both capture modes use the same public v4 export builder, which independently
requires the verified UID. Decrypted-command replay can inspect decoded data,
but cannot establish identity or write a public capture export without the raw
login frames. A fresh baseline is required before publishing its UID, optional
nickname and account-scoped opaque item IDs; see `HSR_EXPORT.md`.
The UID is an account-routing key and the nickname is display text, not proof of
account ownership. Supporting a new inner login schema requires explicit review.
Automatic website sync routes UID-bearing data to the matching account and data
without a UID to Default, independent of the currently viewed profile.
