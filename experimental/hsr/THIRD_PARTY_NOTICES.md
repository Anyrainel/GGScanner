# Third-party notices and research provenance

The `hsr_scanner` library is licensed under GPL-2.0-or-later, as declared in
this crate's `Cargo.toml`. It is linked into the shared GGScanner application
targets and does not produce a separate HSR executable.

## Native inventory and achievement packet boundary

The transport under `src/network/` is adapted from
[IceDynamix/reliquary](https://github.com/IceDynamix/reliquary/tree/d5cf3b7e7e66470d2d8efff6676aa18762b21d3b),
v23.0.0, commit `d5cf3b7e7e66470d2d8efff6676aa18762b21d3b` (2026-08-28,
HSR 4.5). Its MIT license is reproduced in `LICENSE-reliquary.txt`.

The seven inventory record types under `src/packet_capture/proto/` are sourced
from [Reliquary v24.0.0 / HSR 4.6](https://github.com/IceDynamix/reliquary/tree/0b8c1d4f8eb5ae9722b5c3273e36a464b7188a38),
revision `0b8c1d4f8eb5ae9722b5c3273e36a464b7188a38`, under the same MIT license.

Continuous inventory deletion uses Reliquary HSR 4.6 revision
[`938287c5cbba930ad68aa3f7010cf7ad953bc654`](https://github.com/IceDynamix/reliquary/tree/938287c5cbba930ad68aa3f7010cf7ad953bc654),
`src/network/command/command_id.rs` and `proto/PlayerSyncScNotify.rs`, under
the same MIT license. The local decoder requires that reviewed dispatch revision
and command before applying explicit relic or Light Cone instance removals.
The newer HSR 4.7 inventory schemas have not been adopted here.

The local adaptation preserves independent KCP conversations, removes raw
packet/seed trace logging, bounds malformed frames, and infers the login seed
from the protobuf shape instead of the upstream token command ID/field tag.
Inventory classification searches repeated fields inside bounded protobuf
wrappers and validates candidates against bundled public definitions. No
executable offsets, memory reads, injected code, or external helper process
are used. The inner inventory record schemas still have explicit protobuf tags;
this does not promise compatibility with arbitrary inner-schema changes.

[reliquary-archiver](https://github.com/IceDynamix/reliquary-archiver/tree/cb109f17a4a15b7604cfe9d078a8735e7735cd25),
v0.18.0, was used to verify base/path character joining, equipment fields, and
relic main/sub-affix conversion. `assets/packet_affixes.json` is a compact
projection of RelicMainAffixConfig, RelicSubAffixConfig, and
MultiplePathAvatarConfig from the same public GIlore source revision
`8cdb905dc2f8e6fffa9be4eb07af3e34435d6091` used by the bundled reference.
The substat calculation is `count * base + step * stepValue`, with ratios
converted to percentage points like the existing HSR observation contract.

The unchanged `keys/hsr.json` public interoperability dataset comes from
[stardb-exporter](https://github.com/juliuskreutz/stardb-exporter/tree/faac466be74cf234c9855c14dd7d723e557e9971).
SHA-256: `5f45f27da792a7058855cfbea3e2ea39eca39d5976ab37038a45b52cb29a8b7b`.
It contains version-indexed dispatch keys, never account/session data.
The prior auto-reliquary dependency has been replaced by the local audited
transport. Achievement classification remains independent and accepts packed
Quest counters without retaining them in the completed-achievement export.

## Embedded normalized reference

GGScannerOCR and GGScanner embed a compact normalized snapshot generated from
the manifest-verified GIlore/GGStarRail reference boundary at GIlore commit
`1ca018ee615011e61f90f11c3f47738e6ebac30b` and public source revision
`8cdb905dc2f8e6fffa9be4eb07af3e34435d6091`. The source manifest SHA-256 is
`5acf3567d23a9a13531a62c448d075d218e824996ca25e994cef1c322bb89eb1`;
the deterministic embedded document SHA-256 is
`e22c53293b3c2bd3cb4f90b460b699af98ef66f7df9a9f873f7b21ee148208ec`.
The snapshot contains the public identifiers, names, aliases, and gameplay
metadata needed for strict normalization, including 1,921 public achievement
definition IDs. It contains no icon binaries, account/session/capture data,
server item identifiers, or packet data. The source manifest remains the
authority for upstream revision and license-status metadata.

All application flows use the bundled reference automatically. The library's
manifest-verified file provider remains available to offline tooling and tests;
it is not a setting that app users need to configure.

## Interchange format references

- [Reliquary Archiver](https://github.com/IceDynamix/reliquary-archiver/tree/cb109f17a4a15b7604cfe9d078a8735e7735cd25),
  revision `cb109f17a4a15b7604cfe9d078a8735e7735cd25` (v0.18.0), MIT.
- [Reliquary](https://github.com/IceDynamix/reliquary/tree/d5cf3b7e7e66470d2d8efff6676aa18762b21d3b),
  revision `d5cf3b7e7e66470d2d8efff6676aa18762b21d3b` (v23.0.0), MIT.
- [Fribbels Star Rail Optimizer](https://github.com/fribbels/hsr-optimizer/tree/df630a0488a64eeb740e4e0c14f265d96b9f6f8f),
  revision `df630a0488a64eeb740e4e0c14f265d96b9f6f8f`, MIT.

These projects provide the interchange export and protocol references described
below. GGScannerOCR and GGScanner do not download, require, or invoke a
Reliquary Archiver helper.

## GPL-licensed research-only references

- [Kel-Z HSR-Scanner](https://github.com/kel-z/HSR-Scanner/tree/cad008e2509b6b086fd892b140c8b277424fcb03),
  revision `cad008e2509b6b086fd892b140c8b277424fcb03` (v1.5), GPL-3.0.
- [Original YAS Honkai: Star Rail scanner](https://github.com/wormtql/yas/tree/730d8845505c04c7f4c91709de076503401ff40c/yas-starrail),
  revision `730d8845505c04c7f4c91709de076503401ff40c`, GPL-2.0.

These projects were consulted only as historical behavioral references. No
source code, assets, OCR data, or bundled game data from either project was
copied into this crate. HSR-specific geometry, parsing, normalization, and
manager behavior were implemented against this crate's contracts. The runtime
reuses this repository's GPL `yas_core` capture, input, and PaddleOCR
primitives where their contracts match; those components remain covered by the
root project's notices and source license.

## Interchange export

The v4 property/path/slot mapping and character skill anchor interpretation in
`src/scanner_export.rs` and `src/packet_capture/inventory.rs` are adapted from
Reliquary Archiver's `src/export/fribbels/` at the revision cited above. Its MIT
license is reproduced in `LICENSE-reliquary-archiver.txt`. The serialized v4
contract was checked against Fribbels' public `kelzFormatParser.tsx` and
`importConfig.ts` on 2026-09-07. Fribbels implementation code is not bundled.
StarDB's `hsr_achievements` JSON contract was checked against
`juliuskreutz/stardb-exporter/src/ui/achievements.rs` on the same date.
