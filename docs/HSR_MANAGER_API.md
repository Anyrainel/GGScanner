# Star Rail loopback manager API

The Manager tab binds **127.0.0.1:8765** by default (configurable). Only one game
task runs at a time. HTTP I/O runs separately from the existing OCR/mutation
worker, so polling stays responsive while the game is being controlled.
This uses the existing tiny_http dependency, without a browser runtime.

Browser origins: exactly `https://hsr.ggartifact.com`, or loopback development
origins. Genshin's website is not authorized to submit HSR requests. Origin-less
local CLI requests are allowed. Responses include no-store and allowed CORS /
private-network preflight headers. Manage and scan require application/json;
bodies are bounded to 16 MiB. Browser local-network permission may be required.

## Requests

- `GET /health`: `{game:"star-rail",status:"ok",busy,enabled}`.
- `GET /status`: `{game,jobId,kind,phase,count,progress}`. Idle has null job ID
  and kind. Phase: idle, pending, running, completed, failed. Kind: manage/scan.
  Progress contains a bilingual `{zh,en}` message and steps with
  `{key,zh,en,completed,total,showCount,state}`; total may be null. Connection
  and recovery phases have showCount false. Step state is pending,
  running, complete, or interrupted. Errors are shown as the primary message.
- `POST /manage`: the existing validated `goodscanner.hsr.manager-instructions`
  v1 envelope. Requires exact reference identity, the privacy literals, safe
  IDs, and correct semantic SHA-256 idempotency. The explicit website Apply is
  consent for the submitted desired states; it does not grant other operations.
  A visible matcher may occur only once per envelope. GGStarRail keeps repeated
  matchers in its preview counts and omits them from the submitted batch.
  Validation failures return 400 `{error:{zh,en,details}}` before queuing a job;
  details retain the validation code and reason without echoing request data.
- `POST /scan`: `{characters:boolean,lightCones:boolean,relics:boolean}`;
  at least one true. Uses the same settings/runtime/export as local scanning.
  Character requests require the configured Trailblazer nickname and gender.
- `POST /cancel?jobId=...`: safely cancel only the matching task; the server
  remains available for its result and subsequent requests.
- `GET /result?jobId=...`: idempotent result of the latest matching completed
  task. Missing/stale IDs return 404; an unfinished task returns 409.

Accepted jobs return 202 `{game:"star-rail",jobId:"hsr-..."}`. Busy/conflicting
jobs return 409. Repeating the latest completed/pending/running manage envelope
returns the same ID without running twice. An explicit submission after a
failed task gets a new ID and uses the retained journal for recovery. IDs stay
distinct across server restarts. No POST is retried automatically by the client.


### `GET /continuous`

See [Continuous capture API](CONTINUOUS_CAPTURE.md) for the shared endpoint, error
codes, snapshot format, and export policy.

## Results

Manage success:

```json
{
  "jobId": "hsr-...",
  "kind": "manage",
  "verified": 1,
  "needsReview": 0,
  "total": 1,
  "skipped": 0,
  "entries": [],
  "instructions": []
}
```

`entries` contains the existing journal entries (instructionId, exact change,
durable status, attempts, outcome code). `instructions` contains the exact plan
entries and classifications, including already-desired, unmatched, ambiguous,
equipped, unknown-state, locked-discard and changed-before-state exclusions.
Counts do not turn excluded instructions into verified changes. needsReview
is a prominently displayed result requiring manual in-game review.

Scan success is `{jobId,kind:"scan",export:<HSR-Scanner v4>,partial:boolean}`.
It is a versioned export, never implicitly a full inventory replacement.

Failures: `{jobId,error:{zh,en,details}}`; result polling returns 200 with the
correlated error, while phase is failed. The native UI retains the same primary
recovery hint and technical details. Recovery journals remain available on
failure; a verified journal is archived before the next distinct instruction
set. Completed retries never blindly repeat a device click.

No `/equip` implementation is advertised. The CLI's digest/scope confirmation
API remains unchanged; HTTP and CLI share the underlying planner and executor.
