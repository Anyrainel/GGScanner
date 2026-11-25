# Capture failure diagnostics

Both game capture tabs expose listener readiness, elapsed time, received frames,
decoded commands, and a copyable diagnostic summary. Capture diagnostics survive
Stop and failure and reset on a new attempt. A previous transport error can
coexist with successfully decoded commands; it is not proof that every frame failed.

## Audited user flow

| Boundary | Feedback / recovery |
| --- | --- |
| No export targets selected | Start disabled; select export targets |
| Capture-permission check, reference data, dispatch keys, runtime / worker creation | Failure card with complete source chain; startup milestones in logs. Windows checks administrator rights; Linux probes an `AF_PACKET` socket and names the `setcap cap_net_raw+ep` grant when it is missing |
| Native backend, UDP filters, native stream creation | Preserve platform error codes and source chain. Windows uses pktmon; Linux opens raw `AF_PACKET` sockets with an IPv4/UDP BPF filter. Known API / permissions errors retain recovery hints |
| Listener ready, no frames | Visible no-traffic status; verify the game runs locally and start capture before login |
| Frames arriving, no decoded commands | Separate traffic and decoded-message counts; latest transport error and dispatch/session failures in diagnostics |
| Decoded commands, login exchange missing | Handshake remains incomplete; capture a new login |
| Login exchange captured, requested data missing | Per-category progress; open Genshin Achievements when requested; update the scanner if matching remains unsuccessful |
| Five minutes without completion | Terminal timeout with progress / transport diagnostics; retry after correcting the reported stage |
| Native crash, Rust panic, closed stream, unexpected worker exit | Failure card and log; completion cannot be inferred from worker termination |
| Stop / close during initialization or capture | Stop cancels and retains task ownership through cleanup; no export success is reported |
| Unknown reference IDs / missing required export fields | Genshin logs the missing mapping or field and public ID instead of silently dropping a record |
| Debug packet dump cannot be written | Genshin warns and keeps capture running; HSR reports its dump error |
| Serialization, output folder creation, write, sync, filename collision | Failure details; existing exports retained until the new file is saved |
| Retention cleanup fails after saving | Failure details identify the saved file; the new file is retained |
| Completion / retry | Counts reflect the saved export; diagnostics reset for a fresh attempt |

Periodic diagnostic summaries are logged every 15 seconds. GUI logs are written
to `log/run_*.log` relative to the launch working directory; the existing logger
reports disk logging failures in the application. Normal diagnostics contain
counts, protocol/key identifiers, command IDs/lengths, and error descriptions,
not packet bodies or key material. Optional packet dumps are separate.

## Verification limits

Offline tests cover diagnostic classification, selected-target completion,
malformed command / KCP lengths, missing keys, timeout, cancellation, worker
cleanup, UI rendering, export retention, and completion writing exactly once.
Native capture against a real game login still requires a live smoke test on
each platform. Receipt of frames alone cannot establish whether a VPN, virtual
adapter, unsupported packet layout, or game protocol change caused the failure;
the UI reports observed evidence and retains the underlying error for diagnosis.
The Linux backend is unit-tested for frame acceptance (IPv4/UDP only, first
fragment only, ports 22101-22102) but has not been run against a live game.
