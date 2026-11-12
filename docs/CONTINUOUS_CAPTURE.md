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
| 503 | `continuous_capture_no_data` | Enabled, but the initial character and equipment inventory has not been parsed yet. Keep polling. |
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

The listener reuses the game's existing structural packet decoders. Recognized
inventory refreshes and full item-record updates replace records by instance ID.
A single equipment family cannot establish Star Rail's initial inventory; the
structural matcher does not yet prove that a paired inventory batch is an
authoritative full baseline. Websites therefore preserve missing items during
continuous imports rather than infer deletions from a declared complete label.
This is the latest **successfully decoded** inventory: deletion-only GUID messages,
scalar-only lock/equipment changes, and other unsupported notification shapes
are not inferred from arbitrary packets. Re-login provides a fresh baseline.
Live-game validation of those remaining notification families is still needed.
Genshin achievements appear if a recognized achievement packet arrives; they do
not block inventory availability. Star Rail continuous inventory excludes quests.


## Account routing

Enabling automatic sync authorizes the websites to apply subsequent snapshots
without repeated session approval. Identified snapshots route by their game UID,
not the currently viewed profile. Previously unknown UIDs create a profile or
bind the existing unbound Default when no matching UID profile exists. An existing
UID profile always wins over Default promotion; unrelated profiles are retained.

Current Genshin capture does not provide a validated UID or player nickname.
The Genshin website therefore keeps one explicitly enabled fallback profile and
applies unidentified captures there regardless of which account is logged in.
Optional GOOD `metadata.uid` and `metadata.nickname` are supported for identified
imports, but no UID is guessed from arbitrary scalar fields or character names.
See `GENSHIN_EXPORT.md` for the current protocol-validation boundary.

Star Rail's reviewed login layout provides UID and optional nickname from the
same incoming KCP conversation as its inventory. Normal and continuous capture
exports carry them in `metadata`. Other layouts remain unidentified and use the
same selected-profile fallback. Public UID is account identity, not a credential.
No authentication tokens or key material are exported.

Packet captures with a validated UID and aligned instance identities mark
`generator.itemIdentity` as `account-instance-v1`; their `_uid` fields are stable,
opaque per-account equipment identifiers. Legacy/OCR exports retain scan-order
IDs and omit this marker. Clients must not treat those legacy IDs as stable.
