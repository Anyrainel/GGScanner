# Capture key freshness

Both capture decoders receive bundled dispatch-key tables from the application.
Genshin additionally accepts `keys/gi.json` beside the executable. None of these
paths automatically download keys at runtime. A Cargo dependency update does
not update these application-owned files, and the Genshin decoder is patched
to a local vendored copy.

The separate [capture upstream review](CAPTURE_UPSTREAM_AUDIT.md) covers RSA,
decryption, token exchange and protocol-source drift. Its read-only
`scripts/check_capture_upstreams.py` complements this dispatch-table check.

Run `uv run --no-project python scripts/check_capture_keys.py` to compare the
working tree with StarDB Exporter's current public GI/HSR key tables and
Reliquary's HSR dispatch-key table in `src/network/crypto.rs`. The checker
resolves each source branch once and fetches files at those immutable commits.
It strictly parses the reviewed Rust string/`concat!` key arms and merges both
HSR tables; unsupported expressions or conflicting selector values fail closed.
It validates key identifiers, base64 and 4096-byte key lengths. Missing or changed
upstream entries fail; historical local-only entries are preserved. It never
prints key material, modifies files, stages changes or publishes builds.

The installed `.githooks/pre-push` checks every outgoing non-deleted Git tree,
then runs both games' capture tests. Uncommitted local changes cannot make an
outdated outgoing commit pass. Network verification failures block the push.
Enable this hook in new clones with `git config core.hooksPath .githooks`.
The build workflow repeats the key check against its checked-out commit, so
bypassing or omitting local hooks does not produce an unchecked release build.

An outdated table must be synchronized from the reported upstream revision,
validated and committed before retrying. The hook deliberately does not mutate
an already selected push. This check detects upstream key updates; it cannot
detect a new game key that upstream has not published yet, update installed
executables, or verify changes to inventory protobuf fields. Those changes
still require separate protocol updates and capture validation.
