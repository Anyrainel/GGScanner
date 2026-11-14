# Genshin export identity

Genshin OCR and packet capture export GOOD v3. Packet captures, both one-time
and continuous, require a verified login UID and include `metadata.uid` in every
successful export. OCR and previously saved exports may omit identity metadata.

The optional GGScanner extension is:

```json
{
  "format": "GOOD",
  "version": 3,
  "metadata": { "uid": "123456789", "nickname": "Test Traveler" }
}
```

The example is abbreviated and uses fictional identity. Both fields are optional
strings. `uid` means the in-game account UID, not a HoYoverse login identifier,
an equipment GUID, or a generated scanner/session ID. `nickname` is the mutable
in-game display name and must not be used as an account identifier. A producer
must populate identity only from a validated source for the same account and
login as the inventory. One-time packet exports and the `data` object of
continuous responses carry the same verified UID metadata.

Genshin packet capture reads the public game UID from a successful incoming
login response and exports it as `metadata.uid`. The reviewed 7.2 layout uses
`GetPlayerTokenRsp.uid` (field 9), success return code 14, key ID 440 and RSA
server seed 477. The seed in that exact field must decrypt with a bundled login
key to eight bytes before the UID is accepted. This layout matches both the
[published protocol](https://github.com/MeChen618/AstaPS/blob/a49f51aa6f2787ffbc25cee46801b70dee860d23/src/main/proto/GetPlayerTokenRsp.proto)
and a retained official-server login response. Command IDs are not used to
guess identity. The HoYoverse account identifier in field 5 is not the game UID.

Packet headers and bodies are decoded separately. In particular, the legacy
`PacketHead.user_id` schema field is not an identity source; current retained
Genshin headers do not contain it. No UID is inferred from other integers, item
GUIDs, or custom character names. Nickname capture remains optional and is not
currently populated. OCR exports continue to omit identity.

Identity belongs to the active incoming KCP conversation. A login request,
disconnect, or newly verified token response clears both identity and inventory;
reconnecting to the same UID also starts a new continuous snapshot session.
Traffic from retired conversations cannot repopulate the current account.
If the server reuses a retired conversation ID within one running listener,
restart that listener; a reused-ID acknowledgment cannot currently be
distinguished from delayed old traffic.
Both one-time and continuous capture wait for a verified UID and the selected
initial data before completing. An unsupported token identity layout reports a
capture error with instructions to update GGScanner and log in again. Inventory
alone cannot complete capture or produce an anonymous public packet export.

Consumers continue to accept existing GOOD and OCR exports with no metadata.
Automatic website sync routes UID-bearing data to the matching account,
creating one when necessary; data without a UID routes to Default. The currently
viewed profile does not select the destination. Manual imports can offer an
explicit account-binding review. This fallback is an import rule, not permission
for GGScanner packet capture to omit its UID.
