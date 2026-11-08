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

These changes do not alter the upstream cryptographic algorithms or token
recognition. Tests exercise token recognition before inventory, ordinary
dispatch packets, handshake reset and synthetic encrypted KCP frames in both
directions: post-disconnect frames remain blocked until a new login request.
No private capture frames or session material are included in the tests.
