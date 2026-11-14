# Continuous capture API

Available on both manager servers, including the OCR edition (which returns the
`continuous_capture_disabled` error). In the Windows capture edition, enable
**Continuous Sync (Capture) / 持续同步（抓包）** before starting the manager server, then
log into the game. The listener runs until the server stops; OCR and manage jobs
do not replace this separate packet snapshot. Existing origin and preflight
rules apply. Responses use `Cache-Control: no-store`.

| HTTP | `error` | Meaning |
| --- | --- | --- |
| 409 | `continuous_capture_disabled` | This server session has no continuous listener. Enable it before starting the server. |
| 503 | `continuous_capture_no_data` | Enabled, but verified login identity and the initial character/equipment inventory are not ready yet. Keep polling and log in after starting the listener. |
| 503 | `continuous_capture_failed` | Listener initialization, decoding, or capture failed. `details` contains diagnostics. Restart the server after fixing the cause. |
| 200 | — | Latest parsed inventory is available. |

Success wraps the game's normal export, with no job ID:

```json
{
  "sessionId": "a4de65c5-2c68-4246-a04e-f451def99a47",
  "revision": 1,
  "updatedAt": 1793900000000,
  "data": { "format": "GOOD", "version": 3 }
}
```

`data` is a GOOD v3 export for Genshin and the existing HSR-Scanner export for
Star Rail; the example is abbreviated. `updatedAt` is Unix milliseconds when the
export content last changed. `revision` increases only on content changes and is
local to one login generation. `sessionId` changes on server start and each
observed login/account reset, including reconnecting to the same UID. This makes
a reset visible even when it occurs entirely between polls. Clients should poll
every few seconds and import when `(sessionId, revision)` changes. A new login or
disconnect clears the previous inventory to avoid mixing accounts; the endpoint
returns `continuous_capture_no_data` until the next initial inventory is ready.
No prior export file is loaded as live data.

Every approximately ten minutes, changed content replaces a single latest file
in the configured output directory (`genshin_export_latest.json` or
`star_rail_export_latest.json`). The file contains the export itself, without the
HTTP wrapper. Identical exports are not rewritten. File failures leave the
in-memory endpoint available and are retried at the next interval. The first
write occurs at the first interval with data; stopping does not force a write.

The listener reuses the game's existing structural packet decoders. Full
item-record updates replace records by instance ID, so enhancing an item does
not append another copy. Server-confirmed deletion notifications remove known
instances, including enhancement fodder and salvaged/recycled items. A ready
inventory remains available when its final item is removed; empty arrays mean
empty inventory, not missing capture data.

Genshin recognizes the strict StoreItemDelNotify shape: one packed GUID list
and one store-type value (1 or 2), with no other fields. Deletions require the
verified login UID and apply only to held GUIDs. Upgrade responses, strongbox
results and arbitrary GUID-bearing packets must not count as deletions. The
website replaces the supplied inventory sections from the resulting snapshot.

A single equipment family cannot establish Star Rail's initial inventory; the
structural matcher does not prove that a paired inventory batch is an
authoritative full baseline. Its website preserves missing items and applies
only explicit deletions from the reviewed PlayerSyncScNotify layout. The export
includes cumulative `generator.deletedInstanceIds` lists for `relics` and
`lightCones`, using the same account-scoped opaque hashes as `_uid`. These lists
survive missed polls, clear on a new login, and drop an ID if that instance is
observed again. They require `generator.itemIdentity: "account-instance-v1"`
and a verified UID; only that UID's matching captured items may be removed.
Legacy IDs and unrelated absent inventory remain intact. See `HSR_EXPORT.md`
for the extension and `CAPTURE_UPSTREAM_AUDIT.md` for supported protocol versions.

This is the latest **successfully decoded** inventory. Scalar-only
lock/equipment changes and other unsupported notification shapes are not
inferred from arbitrary packets. Fresh live-game validation is still needed;
packet regression tests establish decoder behavior, not every current server
layout. Re-login supplies a fresh baseline.
Genshin achievements appear if a recognized achievement packet arrives; they do
not block inventory availability. Star Rail continuous inventory excludes quests.


## Account routing

Enabling automatic sync authorizes the websites to apply subsequent snapshots
without repeated session approval. Snapshots route by their game UID to the
matching account, creating one when necessary, regardless of the currently
viewed profile. Default and unrelated accounts are retained. Data without a UID
routes to Default. This is the websites' import fallback; successful GGScanner
packet captures always have a verified UID.

Genshin capture provides `metadata.uid` from its verified incoming login
response. It clears identity together with inventory on a new login, including
reconnects to the same UID, and waits for identity plus the initial inventory
before publishing. Unsupported login identity layouts report a capture error.
No player nickname is currently exported. No UID is guessed from
arbitrary scalar fields or character names. See `GENSHIN_EXPORT.md` for the
reviewed protocol layout and validation boundary.

Star Rail's reviewed login layout provides UID and optional nickname from the
same incoming KCP conversation as its inventory. Both one-time and continuous
capture require verified identity and carry the UID in `metadata`. If the login
layout is unsupported, a capture error explains how to update and log in again;
complete inventory alone cannot produce a successful anonymous export.
Public UID is account identity, not a credential.
No authentication tokens or key material are exported.

Packet captures with a validated UID and aligned instance identities mark
`generator.itemIdentity` as `account-instance-v1`; their `_uid` fields are stable,
opaque per-account equipment identifiers. Legacy/OCR exports retain scan-order
IDs and omit this marker. Clients must not treat those legacy IDs as stable.
