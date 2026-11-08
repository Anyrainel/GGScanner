# Genshin export identity

Genshin OCR and packet capture export GOOD v3. Existing exports retain their
current shape: the scanner does not add identity metadata when identity is
unavailable.

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
login as the inventory. Ordinary exports and the `data` object of continuous
responses carry the same optional metadata.

Current Genshin OCR does not read account identity. Current Genshin packet
capture also leaves metadata absent: a trustworthy UID and nickname have not
been verified against a current, correctly framed login replay. The presence of
a field called `user_id` in a bundled protocol schema does not establish that
live game packets populate it. Existing item/avatar fixtures cannot prove that
identity because they do not retain a separate header boundary. No UID or name
is inferred from arbitrary protobuf scalars, item GUIDs, or custom character
names.

Consumers must continue to accept old GOOD exports with no metadata. For
hands-off continuous sync without a validated UID, the website can use the
single profile explicitly selected in its profile-level sync setting. Metadata
support alone does not enable Genshin UID-based automatic profile routing.
