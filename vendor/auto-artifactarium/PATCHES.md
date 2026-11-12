# Local capture-state patch

Source: https://github.com/konkers/auto-artifactarium at
`4ba25fac64b88970143af6bc2a2ef51338e620d0`. Upstream LICENSE and credits are retained.

The pinned source is kept locally to expose `key_exchange_captured()`, a boolean
derived from the token response's successfully decoded session-seed candidates.
No seeds or keys are exposed to the application.

Handshake requests and explicit disconnects now share a connection-state reset:
both KCP directions, the discovered key, session-seed candidates, timestamp and
retained client seed are discarded. A disconnect also blocks subsequent KCP
segments until a new handshake request is observed. Delayed packets from the
closed connection cannot repopulate the cleared inventory while that request is pending.
Initial late-start capture remains allowed as before. The generic established
control event alone cannot reopen a closed stream, because the existing parser
also uses that event for unknown short control codes.

These changes do not alter the upstream cryptographic algorithms. Tests exercise token recognition before inventory, ordinary
dispatch packets, handshake reset and synthetic encrypted KCP frames in both
directions: post-disconnect frames remain blocked until a new login request.
No private capture frames or session material are included in the tests.

## Genshin UID capture and command framing

GameCommand now retains PacketHead and protobuf body separately. Session seed
timing comes only from the header. A complete decrypted command can be rebuilt
with `to_bytes()` so debug dumps retain framing boundaries. Incoming direction
is preserved through KCP; only incoming dispatch responses whose RSA seed was
decoded establish a token response.

The identity reader recognizes the reviewed 7.2 login layout and verifies the
RSA seed in field 477 specifically before accepting success retcode 14 and game
UID field 9. Key ID field 440 must be supported. It rejects malformed, duplicate
or missing identity fields. No account login identifier, authentication token,
seed or key is exported as identity. The source and capture-validation evidence
are recorded in `docs/GENSHIN_EXPORT.md` and `docs/CAPTURE_UPSTREAM_AUDIT.md`.

Both KCP directions share one active conversation. Incoming explicit
establishment controls select it; delayed data and controls from retired
conversations cannot replace or clear it. Retired IDs stay blocked for the
listener's lifetime: an acknowledgment reusing an old ID cannot be distinguished
from delayed old traffic with the validated control information. Restart the
listener if the server reuses such an ID. Cryptographic algorithms remain
unchanged.
