# Download policy

External download candidates start concurrently. There is no IP geolocation
request or country gate: a working direct connection can win from any network,
and a working proxy can win when direct access is blocked or slow.

`genshin/src/download.rs` owns the asynchronous requests and cancellation.
Connections have a 3-second timeout, the complete header/redirect phase has a
5-second budget, and body reads time out after 5 seconds without data. A source
that continues transferring can take up to 300 seconds. The first **complete,
validated** artifact wins; all other futures and requests are dropped. After
3 seconds, if a source has transferred at least 256 KiB, candidates transferring
less than half the leader's bytes are cancelled. If every surviving source then
fails, cancelled candidates are retried once without pruning. If all sources
are slow, none is discarded merely for failing an absolute speed threshold. Fast
error pages and invalid artifacts cannot cancel the remaining downloads.
No files are written by competing requests.

## Sources

- Hosted game data, mappings and achievement catalogs stay on `ggartifact.com`
  and `hsr.ggartifact.com`, with their existing cache/failure policies. They are
  never wrapped in a public download proxy.
- GitHub release executables race direct GitHub, `gh-proxy.com` and `ghfast.top`.
  Edition mapping and staged executable replacement are unchanged.
- Windows and Linux x64 ONNX Runtime 1.22.0 race Huawei's NuGet mirror, official
  NuGet, and those three GitHub release routes.
- Update checks request the official API (direct and via `gh-proxy.com`) and release redirects
  concurrently. Responses are gathered for at most 5 seconds and compared by
  version, rather than accepting the fastest potentially stale cache. The
  pinned-tag `update.json` requests also run concurrently; the largest matching
  revision wins. API asset inventories remain preferred when versions match.

## ONNX Runtime

The same official NuGet package contains both native x64 libraries. Range
requests read the ZIP directory, local header and compressed library payload.
This transfers about 4.56 MiB for Windows or 7.35 MiB for Linux, rather than the
115.78 MiB multi-platform package. If a server ignores Range and returns HTTP
200, the complete ZIP is accepted and extracted within the archive size limit.
Incorrect ranges, truncated archives and oversized entries are rejected.
Windows GitHub release ZIPs also use ranged extraction to skip debug symbols;
unsupported ranged extraction falls back to downloading that complete archive.

Each platform's SHA-256 is pinned in `runtime_download.rs`. These hashes were
verified against Microsoft's NuGet **and** GitHub release binaries. The winning
library is staged next to the destination, flushed, and renamed into place only
after validation. Updating the runtime requires updating its version, platform
entry names and verified hashes together. Local/system library detection is
unchanged; file presence is not an inference compatibility test.

## Verification

Routine tests use local HTTP servers to cover competing requests, cancellation,
invalid content, read stalls, size limits, ranged extraction and full-ZIP fallback:

```sh
cargo test -p genshin_scanner --lib download
cargo test -p genshin_scanner --lib updater::tests
```

Opt-in live checks fetch both native libraries from Huawei and official NuGet,
verify their hashes and install nothing:

```sh
cargo test -p genshin_scanner --lib live_nuget_both_platforms -- --ignored --nocapture
cargo test -p genshin_scanner --lib live_windows_runtime_race -- --ignored --nocapture
# On Linux:
cargo test -p genshin_scanner --lib live_linux_runtime_race -- --ignored --nocapture
```

Successful live checks on a developer network establish artifact availability
and integrity, not performance across mainland China telecom networks.
