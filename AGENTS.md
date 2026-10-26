# Yas — Genshin Impact Scanner

## Build Rules

- When a running scanner executable locks a build output, the user authorizes stopping that scanner process before rebuilding and reopening it.

## Public Releases And Player-Facing Changes

- **Treat the public release page, download list, and release copy as product UI.**
  A technically correct updater or passing build does not establish that the
  release is ready for players.
- **Technical needs must fit the public product contract.** Build identity,
  cache handling, compatibility, and internal naming are implementation concerns.
  Solve them through metadata, tags, manifests, or internal routing; do not expose
  extra downloads, choices, or explanations merely to simplify implementation.
- A routine release publishes the existing editions. Do not add public variants,
  duplicate binaries, revision-suffixed executables, or rename downloads unless
  the user specifically requests that public change. The current release asset
  Windows editions are `GGScanner.exe` (capture + OCR + manager) and
  `GGScannerOCR.exe` (OCR + manager). `update.json` maps only to these canonical
  names. Until 2026-11-21, releases also include temporary `GOODCapture.exe`
  and `GOODScanner.exe` compatibility copies; remove them at the cutoff.
- Keep build revisions in version/manifest metadata. The updater and website
  must use the same download for each edition. Preserve edition mapping when
  handling historical names. New clients and websites prefer canonical names;
  the explicitly authorized compatibility copies expire after one month.
- Same-day publication is blocked when that day's release or tag already exists.
  Never automatically delete, overwrite, or add another public build variant.
  A replacement requires the operator to manually remove the existing release
  and tag first. Ordinary CI builds may still run without publishing.
- Before publishing, inspect the exact proposed asset list and bilingual copy
  from a player's perspective. Each download must represent a distinct useful
  choice. Check names, edition mapping, links, manifest targets, and unexpected
  files. After publishing, verify the actual release against that same contract.
- Write release/download copy around supported games, benefits, and edition
  choices. Omit control locations, architecture, dependency details, renaming
  plans, and updater mechanics. Use useful download labels and a brief risk
  disclaimer; do not add terms-of-service links or lengthy defensive copy unless
  requested.
- Tests must enforce the intended public contract, not just confirm a workaround.
  When changing release plumbing, verify the asset inventory and updater targets
  as well as build success. Passing tests cannot excuse a confusing public result.
- Existing code, tests, and rollout docs are evidence of past decisions, not
  automatic justification for retaining them. Reassess inherited workarounds
  against the user's current requirements, and update obsolete instructions
  rather than turning implementation accidents into permanent policy.

## Prioritization

- **When instructions are clear, implement them directly.** Do not block one clear item because another item needs discussion. Make all clear changes first, then discuss the unclear ones.

## Logging And UI Status

- Rust logs must use `yas::log_debug!`, `log_info!`, `log_warn!`, or `log_error!` with two literal templates in the order `(Chinese, English)`. The macro selects one language; never put `中文 / English` in either template or in a dynamic value.
- Keep errors language-neutral until they reach a user-facing boundary. If a legacy bilingual `anyhow` error must be logged, call `yas::lang::localize(&error.to_string())` before passing it as a log argument. Do not log `anyhow` chains containing bilingual text directly.
- Use logs for diagnostics and failures, never to update visible task progress. Scanner progress goes through `ProgressFn` and category lifecycle through `ScanPhaseFn` (`genshin/src/scanner/common/progress.rs`). GUI task status uses `TaskProgress`/`Step`/`StepState` (`application/src/gui/task_progress.rs`), with worker updates in `application/src/gui/worker.rs`.
- For a current phase message or user-facing failure, use `UiText` and `TaskStatus` (`application/src/gui/state.rs`) so the GUI selects one language at display time; keep technical details separate for copying.
- UI copy uses the existing language pair at the presentation boundary (`Lang::t(zh, en)` in the GUI); do not build bilingual strings with a slash. Progress callbacks carry counts, item IDs, and phase keys, not translated prose when a stable key is available.

## No Piecemeal Refactoring

- **When receiving feedback that changes a design decision, propagate its full implications** — don't patch only the specific spot the user pointed at. If the feedback implies a different ownership model, naming scheme, or data flow, update all affected sites, not just the one mentioned.

## DRY is Top Priority

- **Never duplicate logic between test binaries and production code.** Core features (UI navigation, OCR scanning, filter operations, grid scanning) must be implemented as well-defined methods in the proper modules (e.g., `manager/ui_actions.rs`). Test binaries should only contain test-specific looping/reporting logic and call shared functions.
- When refactoring for DRY, **ensure logic equivalence** — the extracted function must behave identically to the original inline code.
- Prefer reusing existing functions over writing new code that does the same thing.

## Overview

Yas (Yet Another Scanner) is a Rust application that scans Genshin Impact in-game data (characters, weapons, artifacts) using OCR and exports it in **GOOD v3** (Genshin Open Object Description) format for use with optimizer tools.

## Architecture

### Workspace Crates

- **`yas`** (`yas_core`) — Platform-agnostic core library: screen capture, OCR (PaddlePaddle ONNX models), system control (mouse/keyboard), game window detection, positioning/scaling utilities.
- **`genshin`** (`genshin_scanner`) — Genshin-specific scanner logic: GOOD v3 scanners for characters, weapons, and artifacts. Handles in-game navigation, panel OCR, and name matching via remote mappings.
- **`application`** (`good_tools_app`) — Binary crate. Two user-facing targets: `GGScannerOCR.exe` (OCR scanner + manager) and `GGScanner.exe` (capture + OCR scanner + manager, behind the `capture` feature flag).

### Key Modules (genshin)

```
src/
├── cli.rs                     # CLI entry point, orchestrates all scanning + run_server_core()
├── server.rs                  # HTTP server (tiny_http): /manage, /equip, /status, /result, /artifacts
├── updater.rs                 # Auto-update: GitHub release check + self-replace
├── manager/                   # Artifact lock/equip manager (server-driven)
│   ├── orchestrator.rs        # ArtifactManager: top-level execute() and execute_equip()
│   ├── lock_manager.rs        # LockManager: single-pass backpack scan + per-page lock toggle
│   ├── equip_manager.rs       # EquipManager: equip/unequip via character screen navigation
│   ├── matching.rs            # Hard-match artifacts: all fields + 0.1 substat tolerance
│   ├── models.rs              # Request/response types: LockManageRequest, EquipRequest, ManageResult
│   ├── ui_actions.rs          # Game UI helpers: click lock button, open character screen, etc.
│   └── mod.rs
├── scanner/
│   ├── common/                # Shared scanner infrastructure
│   │   ├── game_controller.rs # Mouse/keyboard/capture control
│   │   ├── backpack_scanner.rs# Grid-based inventory navigation
│   │   ├── mappings.rs        # Remote name→GOOD key mappings (from ggartifact.com)
│   │   ├── coord_scaler.rs    # Resolution-independent coordinate scaling (base: 1920x1080)
│   │   ├── models.rs          # GOOD v3 data models (GoodExport, GoodCharacter, etc.)
│   │   ├── stat_parser.rs     # Artifact stat string parsing
│   │   ├── diff.rs            # Groundtruth comparison tooling
│   │   ├── constants.rs       # Grid positions, UI coordinates
│   │   ├── ocr_factory.rs     # OCR backend selection (ppocrv3/v4/v5)
│   │   ├── ocr_pool.rs        # Channel-based pool of N OCR model instances
│   │   ├── pixel_utils.rs     # Color/pixel analysis helpers
│   │   ├── fuzzy_match.rs     # Fuzzy string matching for OCR results
│   │   └── navigation.rs      # Tab/page navigation helpers
│   ├── character/              # Character panel OCR
│   ├── weapon/                 # Weapon panel OCR
│   └── artifact/               # Artifact panel OCR (scanner.rs has identify_artifact, scan_level_only)
```

### Key Modules (application)

```
src/
├── main.rs                    # Entry point: CLI mode or GUI mode (GGScannerOCR.exe)
├── bin/
│   └── yas.rs                 # Entry point for GGScannerOCR.exe and GGScanner.exe
└── gui/
    ├── mod.rs                 # eframe App impl, tab routing
    ├── state.rs               # AppState: all GUI state fields
    ├── worker.rs              # spawn_scan(), spawn_server() — background thread launchers
    ├── manager_tab.rs         # Manager tab UI: server start/stop and requested-set filtering
    ├── scan_tab.rs            # Scan tab UI: scan target checkboxes, options
    ├── capture_tab.rs         # Capture tab UI: start/stop packet capture, export
    ├── settings_tab.rs        # Settings tab: config editing
    └── log_tab.rs             # Log viewer tab
```

### Key Modules (genshin/capture — behind `capture` feature flag)

```
src/capture/
├── mod.rs
├── packet_capture.rs          # UDP capture via pktmon on ports 22101–22102
├── monitor.rs                 # CaptureMonitor: orchestrates capture, decryption, data accumulation
├── data_cache.rs              # Downloads/caches data_cache.json from ggartifact.com
├── data_types.rs              # DataCache types (irminsul/anime-game-data format)
├── player_data.rs             # PlayerData: converts captured packets → GOOD v3 export
└── testdata/                  # Binary test fixtures (items.bin, avatars.bin, noise.bin)
```

### How Scanning Works

1. User opens Genshin Impact and navigates to the appropriate screen
2. `GenshinGameController` captures the game window and provides scaled coordinates
3. `BackpackScanner` navigates the grid inventory (weapons/artifacts)
4. Individual scanners OCR each panel's fields (name, level, stats, etc.)
5. OCR results are fuzzy-matched against `MappingManager` data (fetched from ggartifact.com)
6. Results are exported as GOOD v3 JSON

### Config File (`good_config.json`)

On first run, a bilingual prompt asks for custom in-game names for Traveler/Wanderer/Manekin/Manekina (renameable characters). The JSON file is created next to the exe with these names plus all timing/delay defaults:

```json
{
  "traveler_name": "",
  "wanderer_name": "",
  "manekin_name": "",
  "manekina_name": "",
  "char_tab_delay": 500,
  "char_next_delay": 300,
  "char_open_delay": 1500,
  "char_close_delay": 500,
  "inv_scroll_delay": 200,
  "inv_tab_delay": 400,
  "inv_open_delay": 1500,
  "capture_delay": 40
}
```

Existing config files without delay fields are loaded correctly via `#[serde(default)]` and re-saved with new defaults. Old per-scanner field names (`weapon_grid_delay`, `artifact_grid_delay`, etc.) are accepted via serde aliases for backwards compatibility.

## Build & Run

```bash
# Stable Rust toolchain
rustup default stable

# Build
cargo build --release

# The binary is at target/release/GGScannerOCR.exe
# Run with default (scan artifacts):
GGScannerOCR.exe

# Scan everything:
GGScannerOCR.exe --all

# Scan specific categories:
GGScannerOCR.exe --characters --weapons --artifacts
```

Requires administrator privileges on Windows (for input simulation).

## CLI Flags

All help text is bilingual (Chinese + English). Flags are grouped into four sections:

### Scan Targets
- `--characters` / `--weapons` / `--artifacts` / `--all`

### Global Options
- `-v, --verbose` — detailed scan info
- `--stop-on-failure` — stop at the first OCR failure; default skips unreadable entries and logs the cause
- `--log-progress` — log each scanned item
- `--output-dir <DIR>` — output directory (default: `.`)
- `--ocr-backend <NAME>` — override every category's secondary (v5-slot) backend: character name/level check, weapon equip fallback, artifact level (also achievements)
- `--dump-images` — save OCR region screenshots to `debug_images/`

### Scanner Config
- `--weapon-min-rarity <N>` — min weapon rarity (default: 3)
- `--artifact-min-rarity <N>` — min artifact rarity (default: 4)
- `--char-max-count <N>` / `--weapon-max-count <N>` / `--artifact-max-count <N>` — max items (0 = unlimited)
- `--weapon-skip-delay` / `--artifact-skip-delay` — skip panel delay (faster but less reliable lock/astral detection)
- `--char-ocr <NAME>` — character general backend (default: ppocrv4)
- `--weapon-ocr <NAME>` — weapon general backend (default: ppocrv6tiny)
- `--artifact-substat-ocr <NAME>` — artifact substat/general backend (default: ppocrv6tiny)

### Debug
- `--debug-compare <PATH>` — groundtruth JSON comparison
- `--debug-actual <PATH>` — offline diff (no scanning)
- `--debug-start-at <N>` — skip to item index
- `--debug-char-index <N>` — jump to character index
- `--debug-timing` — per-field OCR timing
- `--debug-rescan-pos <R,C>` — re-scan a grid position
- `--debug-rescan-type <TYPE>` — scanner type for re-scan (default: weapon)
- `--debug-rescan-count <N>` — re-scan iterations (0 = infinite until RMB)

### Architecture Notes
- Character names are set via first-run prompt → `good_config.json` only (no CLI flags)
- Timing/delay settings live in `good_config.json` only (no CLI flags)
- Per-scanner verbose/dump/continue/log flags consolidated into global flags
- Per-scanner configs are plain structs (no clap derives); the orchestrator (`cli.rs`) populates them from global CLI flags + JSON config

## Dependencies & Platform

- **OCR**: ONNX Runtime (`ort` crate) with PaddleOCR models (embedded via `include_bytes!`)
- **Screen capture**: `screenshots` crate (with Win32 BitBlt primary path on Windows)
- **Input simulation**: `enigo` crate
- **Remote mappings**: `reqwest` (blocking HTTP to ggartifact.com)
- **Windows only**: Requires admin, uses Win32 APIs for window detection; packet capture (`capture` feature) is Windows-only (pktmon)
- **Linux** (experimental): native build supports OCR scanning + manager. See "Linux Platform Layer" below.

## Platform Architecture

- Select the OS once through Rust's built-in `cfg(target_os)` and target-specific
  Cargo dependency tables. Do not add combinable `windows`/`linux` Cargo features
  or duplicate scanners/controllers for each OS.
- `yas::window::NativeWindow` is the non-owning native window boundary. Its private
  Windows/Linux backends implement geometry, liveness, foreground checks and
  activation. `FocusRequest::Pending` requires later foreground verification;
  a compositor accepting an activation request does not mean focus was granted.
- `yas::capture::create_capturer` owns native capture construction and HDR support.
  `yas::system_control::prepared_control` owns input initialization/preflight.
  Pointer drag sequencing/easing lives once in `system_control/gesture.rs`; OS
  backends provide primitives and validated timing data only.
  Shared game code owns navigation, focus retry policy, OCR, matching and export.
  Keep game-specific window identity checks in the respective game module.
- Both OS builds use the same ONNX Runtime model loading, preprocessing, inference
  and CTC decoding. Model choices are recognition settings, not OS implementations.
- Cargo features represent real product/development capabilities: `capture`,
  `dev-tools`, `test-as-invoker`, and core `ort`. Retired capture backend switches,
  Tract inference and unused streaming/libwayshot capture paths are removed.
- User scans always retain partial results; manager scans require complete data.
  Do not restore the retired persisted `save_on_cancel` / `scanSaveOnCancel`
  preferences or a second Genshin capture-method option derived from HDR.

## Linux Platform Layer

- Linux-only code and dependencies must be isolated with `cfg(target_os = "linux")` and target-specific Cargo tables. Compare the Windows third-party dependency package/features set against main before pushing; the checker normalizes only the two retired local capture switches.
- Build `GGScannerOCR` on Linux. The current shared GUI, Genshin OCR/manager and experimental HSR OCR/attended manager use X11/XWayland windows. Native wine-wayland windows, HDR, packet capture and self-update are unsupported. Do not advertise stubbed capture as available.
- `yas/src/utils/linux_x11.rs` owns discovery, root-coordinate geometry, focus, TrueColor capture decoding and XTEST; `system_control/linux/` owns XTEST/uinput injection. Linux input access is checked before navigation. No ydotool daemon is used.
- Wayland uinput needs explicit device permissions; group membership alone does not grant access on every distro. See `docs/LINUX.md` for setup, boundaries and validation evidence.
- Font discovery and the portal theme watcher compile only on Linux. Keep Windows font selection, system theme handling, scanning timing and stop policy unchanged.
- Linux CI uploads an experimental build artifact only. Public release assets are `GGScanner.exe`, `GGScannerOCR.exe`, and `update.json`; temporary compatibility copies expire on 2026-11-21 as specified above.

## Conventions

- All UI coordinates use 1920x1080 as base resolution, scaled at runtime via `CoordScaler`
- Chinese (zh_CN) game client only — OCR models trained on Chinese game text
- GOOD v3 format spec: keys use PascalCase (e.g., `"SkywardHarp"`, `"Furina"`)
- The `data/` directory (gitignored) caches remote mapping files

## Manager & HTTP Server

### Architecture

Two-thread model: HTTP thread (tiny_http) handles requests, execution thread owns the game controller and processes jobs sequentially. Communication via `mpsc` channel + `Arc<Mutex<JobState>>`.

### Data flow

1. Client sends `POST /manage`, `POST /equip`, or `POST /scan` → server validates, returns 202 with `jobId`
2. Client polls `GET /status` for progress
3. Execution thread processes the job (manage/equip/scan)
4. Client fetches `GET /result?jobId=xxx` (idempotent) for final results
5. Client fetches scanned data via `GET /characters?jobId=xxx`, `GET /weapons?jobId=xxx`, `GET /artifacts[?jobId=xxx]`

### API Reference

Endpoints (summary only — full contract, request/response shapes, status codes, and worked examples live in [`docs/MANAGER_API.md`](docs/MANAGER_API.md); update that file when the wire contract changes):

- `POST /manage` — lock/unlock artifacts. Returns 202 with `jobId`.
- `POST /equip` — equip/unequip artifacts. Returns 202 with `jobId`.
- `POST /scan` — OCR scan for characters/weapons/artifacts. Returns 202; one `jobId` covers all requested categories.
- `GET /status` — poll job state. Manage/equip expose `progress` (linear, per-item). Scan exposes `scanProgress` (per-category, one slot each, `pending`/`running`/`complete`/`aborted`).
- `GET /result?jobId=xxx` — final per-instruction results + summary.
- `GET /characters?jobId=xxx`, `GET /weapons?jobId=xxx` — scan data. 503 if that jobId attempted the category but didn't finish.
- `GET /artifacts[?jobId=xxx]` — scan data or manage snapshot. `jobId` optional for back-compat.
- `GET /health` — `{status, enabled, busy, gameAlive}`.

#### `GET /health` — Health check
Returns `{"status":"ok","enabled":bool,"busy":bool,"gameAlive":bool}`.

### Data Caching

Each data type (characters, weapons, artifacts) has an independent `ScanDataCache<T>` storing the latest `(jobId, data)` plus an `incomplete_job_id` slot. All-or-nothing: a scan category populates the cache only if it completes in full during that run; if it aborts/errors/is never reached, the jobId is recorded as incomplete and the cache isn't written — queries for that jobId return 503. Categories the client didn't request leave the cache untouched. Manage/equip jobs that modify in-game state invalidate the artifact cache before execution.

`--stop-on-failure` is the only OCR-error stop policy: enabled stops on the first failed item; disabled logs, skips the item, and continues. There is no independent consecutive-error threshold.


### Key config flow

GUI `state.filter_involved_sets` defaults true. Management always stops once
targets are matched; it does not refresh full inventory. The independent scan
actions supply inventory data. These choices pass through `cli.rs::run_server_core()`
to `ArtifactManager::new()` and `LockManager::execute()`. See
`docs/SCANNER_OPTION_PARITY.md` for shared options and remaining Star Rail work.

### Matching (matching.rs)

All fields are hard-match (reject on mismatch): set, slot, rarity, level, main stat, elixir_crafted, substats, unactivated substats. Substat values allow 0.1 tolerance for OCR rounding. `location`, `lock`, `astral_mark` are NOT matched (they change independently of artifact identity).

### Lock toggle flow (lock_manager.rs)

Per-page: scan all items via pipelined OCR → match against targets → re-click matched positions → toggle lock → verify pixel. Page-skip optimization: in fast mode, OCR the last item's level first; if > max target level, skip the page entirely (inventory sorted by level descending).

Ordinary artifact scanning, manager page probes, and manager target re-selection
share `scanner/common/artifact_panel.rs`. It waits for changed, readable details,
target selection, and cross-frame stability; only a full timeout permits bounded
retry/reference clicks. OCR receives the accepted screenshot, never an unchecked
timeout capture. Page-local detail fingerprints also guard manager re-selection;
invalidate this cache after navigation. Weapon fixed-delay waits are separate.

### Snapshot (orchestrator.rs)

The internal complete-scan path can build a post-toggle snapshot, but current
management always stops after matching and does not publish a full inventory
snapshot. Use `POST /scan` to populate `GET /artifacts?jobId=xxx`.

## Fuzzy Matching (`fuzzy_match.rs`)

5-tier fallback for matching OCR text against name→key maps:

1. **OCR confusion substitution** — char-by-char replacement of known misreads (e.g., 稚→薙, 拉→菈). Tries each pair individually, then applies ALL applicable substitutions simultaneously (needed when OCR garbles multiple chars, e.g. 菈乌玛→拉鸟玛 requires both 拉→菈 and 鸟→乌).
2. **Exact match** on cleaned/normalized text
3. **Substring match** (both directions: OCR added noise, or OCR truncated)
4. **Levenshtein distance** (30% threshold, char-level for CJK)
5. **LCS uniqueness fallback** (≥2 shared CJK chars, unique to one candidate)

### Adding OCR Confusion Pairs

In `OCR_CONFUSIONS` array. Rules:
- Only add `(wrong, correct)` where `wrong` does NOT appear as a standalone char in any legitimate name — otherwise exact match on that name would never be reached (the substitution would mangle it). Even if the substitution doesn't match, it wastes a lookup. Chars with collisions (菈↔莱, 鹮↔鹤/环) rely on Tier 4/5 instead.
- All current pairs are single-char to single-char. The combined pass assumes this.
- The combined pass applies all substitutions in one char-by-char sweep, avoiding cascading issues with bidirectional pairs (e.g., 茲↔兹).

## Artifact Scanner Details

### Dual-Engine OCR Pipeline

`SharedOcrPools` holds one pool pair per category (character / weapon / artifact); each pair has a general (v4-slot) and a secondary (v5-slot) pool, and pairs naming the same backend share instances. Defaults live in `ocr_pool.rs` (`DEFAULT_*_OCR`), chosen from a live eval against groundtruth:
- **Characters**: general ppocrv4 (`--char-ocr`), secondary ppocrv5. v6 tiny read names 70.7% / levels 67.5% vs v4's 94.7% / 99.2%, and its dictionary lacks 魈.
- **Weapons**: ppocrv6tiny in both slots (`--weapon-ocr`). Name 97.5% vs v4's 89%.
- **Artifacts**: level engine ppocrv6tiny, general engine ppocrv6tiny (`--artifact-substat-ocr`) for name, main stat, set, equip, substats. Substats 100% vs v4's 96.4%, set name 99.2% vs 93.7%; level tied at 100% (v4 alone is only 39.4% on level).
- `--ocr-backend` overrides every category's secondary slot. The equip manager reads character names, so it uses the character pair.

Level uses dual-engine (tries both, takes max valid). Substats use only the general engine. Results are collected as `OcrCandidate` lists per line, then validated by the roll solver.

Second-engine reads (level cross-read, equip retry, substat same-rect retry, character name cross-check, weapon equip fallback) are skipped when both engines run the same model (`ocr_factory::same_model`, via `ImageToText::model_id`) — re-running identical weights on an identical crop cannot change the result. Retries on a different crop (shifted/narrowed substat rects, icon-masked, grayscale set name, other Y probes) always run.

### Roll Solver (`roll_solver.rs`)

Validates substat combinations against game mechanics:
- Uses pre-computed **rollTable** lookup (from `rollTable.json` via `roll_table.rs`) — NOT brute-force f64 enumeration
- Each entry is `(display_value×10: i32, roll_count_bitmask: u8)`, binary searched
- Validates total roll count = init_count + level/4
- **Init preference**: Level 0 → prefer higher init first (lines = init count); Level > 0 → prefer lower init (better accuracy)
- Outputs `totalRolls`, `initialValue` per substat, and `inactive` flag
- The solver treats inactive (待激活) substats identically to active ones — their values are real roll values

### Elixir Crafted Detection

Elixir artifacts display a purple banner ("祝圣之霜定义") that shifts all content down by 40px (`ELIXIR_SHIFT`).
- Detection: 3 pixels at (1510, 1520, 1530), y=423 — checks for purple (blue > 230 && blue > green + 40)
- **Do NOT move to x=1683** — that hits the lock icon and causes massive false positives
- When detected, all subsequent OCR regions are Y-shifted by 40px

### Substat Crop Regions

- Lines 0–2: width 255px (calibrated to avoid OCR noise from wider crops)
- Line 3: width 355px (wider to capture "(待激活)" text on unactivated substats)
- All start at x=1356

### Unactivated Substats (待激活)

- Appear on level-0 artifacts as the 4th substat line with muted font and "(待激活)" appended
- The stat key and value are real (not zero) — it's the value that WILL be added on first level-up
- `stat_parser.rs` detects "(待激活)" text and sets `ParsedStat.inactive = true`, keeping the real value
- `OcrCandidate.inactive` propagates through the solver to `SolvedSubstat.inactive`
- Scanner splits solver results into `substats` (active) and `unactivated_substats` (inactive) in the output

### Pixel-Based Detection (highly reliable)

- **Rarity**: Star pixel color at fixed Y positions
- **Lock**: Pixel color at `ARTIFACT_LOCK_POS1` (1683, 428)
- **Elixir**: Purple banner check at (1510–1530, 423)
- **Astral mark**: Pixel at `ARTIFACT_ASTRAL_POS1`

### Parallelization

- `OcrPool`: Channel-based pool of N OCR model instances
- `scan_worker`: Generic parallel worker for backpack grid items
- **ALWAYS create separate pools** for main and substat OCR (sharing causes deadlock: N tasks each hold 1 instance, all waiting for a 2nd)

## GGScanner (Packet Capture Scanner)

GGScanner is the merged binary (`GGScanner.exe`) with Capture, Scanner, and Manager tabs. It exports GOOD v3 data either by sniffing game network packets or by OCR. The standalone GGScannerOCR.exe remains available for users who do not need packet capture.

### Build

```bash
cargo build --release --features capture --bin GGScanner
```

### How It Works

1. Uses `pktmon` (Windows packet monitor) to capture UDP traffic on ports 22101–22102
2. `GameSniffer` (from `auto-artifactarium` crate) decrypts packets using dispatch keys from `keys/gi.json`
3. `CaptureMonitor` uses **heuristic field-number-agnostic matching** — parses outer protobuf as generic `Unk`, tries every repeated length-delimited field as `Item` or `AvatarInfo`, picks the best match. This survives both command ID rotation AND outer field number changes across game versions.
4. Auto-stops when both character and item packets are received
5. `PlayerData` converts captured data → GOOD v3 JSON

### Dispatch Keys (`keys/gi.json`)

- `HashMap<u16, String>` mapping game version → base64-encoded key
- External key file (`keys/gi.json` next to exe) overrides embedded keys, allowing updates without recompiling

### Dependencies (capture-only)

- `auto-artifactarium` — packet decryption + protobuf types (from konkers/auto-artifactarium)
- `pktmon` — Windows packet monitor driver interface
- `protobuf` — protobuf parsing
- `tokio` — async runtime for capture loop

## Testing & Validation

### Groundtruth

- `genshin_export.json`: Exported via third-party tool, contains complete artifact/character/weapon data
- Note: GT uses typo `elixerCrafted` (not `elixirCrafted`) — diff report handles both

### Diff Report (`diff_report.py`)

- Compares scan output against groundtruth with Hungarian algorithm matching
- Groups by `(setKey, slotKey, rarity, lock)` — rarity and lock are hard matching requirements (pixel-based, very reliable)
- Three-tier categorization: non-stat diffs, stat-key diffs, stat-value-only diffs
- Always run scans with `--dump-images` so dump images match the scan output
- Use `python diff_report.py <scan.json> <gt.json>` to generate `diff_report.md`

### Other Scripts

- `test_solver.py`: Validates roll solver against groundtruth (expects ~99.7% totalRolls accuracy)
- `gen_roll_table.py`: Generates `roll_table.rs` from `rollTable.json`

### Key Calibration Values

| Parameter | Value | Notes |
|-----------|-------|-------|
| Substat width (lines 0–2) | 255px | Wider causes OCR failures |
| Substat width (line 3) | 355px | Captures "(待激活)" text |
| delay_after_panel | 100ms | Lock/astral mark animation |
| Talent overview width | 90px | Supports 2-digit levels |
| ELIXIR_SHIFT | 40px | Purple banner height |
| Elixir pixel positions | (1510–1530, 423) | Do NOT use x=1683 |
