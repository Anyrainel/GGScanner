# Linux port (experimental)

`GGScannerOCR` is the native Linux executable, with the current shared GUI and
CLI. Linux builds are CI artifacts, not additional public release downloads.
CI packages the executable and this guide in a tar.gz to preserve Unix executable
permissions. Public Windows downloads are `GGScanner.exe`, `GGScannerOCR.exe`, and
`update.json`, plus old-name compatibility copies until 2026-11-21.
Do not run a Windows updater asset on Linux.

## Mode boundaries

| Mode | Linux implementation | Validation limit |
| --- | --- | --- |
| Genshin characters, weapons, artifacts, achievements OCR | X11 capture + XTEST/uinput, existing scanners and GOOD v3 export | No live-game run in this review |
| Genshin manager and HTTP scan API | Same controller and job/cache contracts | Core/API tests; no live lock/equip run |
| Star Rail characters, light cones, relics OCR | Native device, shared OCR/scanner and HSR-Scanner v4 export | Experimental; simulated tests and X11 device fixture |
| Star Rail attended relic manager | Same native device, geometry/focus checks and existing journal/plan rules | No live-game mutations tested |
| Packet capture, either game | Windows only; Linux GUI hides capture even with `capture` enabled | Unsupported backend returns a platform error |
| HDR / native wine-wayland game windows | Unsupported | Use SDR and Wine/Proton X11/XWayland |
| Self-update | Skipped on Linux | Rebuild or replace the native binary manually |

Keep the game mapped, in front and unobscured. Core X11 `GetImage` does not
guarantee valid pixels for obscured windows on every compositor. Native Wayland
GUI rendering is supported, but the *game* must still be an X11/XWayland client.
Remove `PROTON_ENABLE_WAYLAND=1` / Wine `Graphics=wayland` if discovery cannot see
the game. Genshin requires the Simplified Chinese client and 16:9; Star Rail
retains its existing language, semantic-validation and minimum-size rules.

## Build and launch (Ubuntu 24.04, x86_64)

```sh
sudo apt-get update
sudo apt-get install -y build-essential pkg-config libssl-dev libxdo-dev \
  libxcb1-dev libxrandr-dev libwayland-dev libxkbcommon-dev fonts-noto-cjk \
  xvfb xauth
rustup toolchain install 1.95.0 --profile minimal --component rustfmt
git lfs pull
cargo build --locked --release --bin GGScannerOCR
./target/release/GGScannerOCR
./target/release/GGScannerOCR --all
./target/release/GGScannerOCR star-rail --help
```

Do not launch the GUI with `sudo`. System CJK fonts are discovered through
fontdb only on Linux, including the collection face index. Missing fonts should
be installed through the distribution, for example `fonts-noto-cjk` above.

ONNX Runtime detection uses `ORT_DYLIB_PATH`, an executable-adjacent
`libonnxruntime.so`, then the system loader. Supply a compatible CPU ONNX Runtime
1.22 library. Otherwise the GUI downloads it; the CLI prompts first. Automatic
download is x86_64 only. Huawei/NuGet native libraries and GitHub release mirrors
race concurrently, with SHA-256 verification before installation. NuGet Range
requests retrieve only the Linux library; release archives supply the versioned
regular library, not its empty symlink. See [download policy](DOWNLOADS.md).
A configured library
still has to load and support the models: file detection alone is not an
inference test. An incompatible system install must be replaced or overridden
with `ORT_DYLIB_PATH`.

## Input access

X11 prefers XTEST after checking that fake motion actually moves the pointer.
Wayland prefers direct `/dev/uinput`, with XTEST fallback if it works. No
ydotool service is used. Test discovery with:

```sh
cargo run -p yas_core --example x11_probe
```

If uinput is needed, first inspect `ls -l /dev/uinput` and distribution policy.
Membership in `input` alone does not grant access on every distribution. An
administrator can use a dedicated group and a rule such as
`/etc/udev/rules.d/70-goodscanner-uinput.rules`:

```udev
SUBSYSTEM=="misc", KERNEL=="uinput", GROUP="goodscanner-input", MODE="0660"
```

Create that group, add the intended user, load the `uinput` module if needed,
reload/trigger the rule, then log out/in. Access grants system-wide input
injection; scope it to the intended user. The scanner reports unavailable
input before navigation begins.

The virtual pointer uses relative motion with position readback and speed
calibration; unverifiable moves fail instead of silently clicking elsewhere.
Use a flat acceleration profile. The optional virtual touchscreen handles
absolute clicks; `GOODSCANNER_TOUCH_CLICK=0` selects the verified pointer path.
Multi-output Wayland touchscreen mapping and fractional scaling vary by
compositor and remain unvalidated. Use a single output for initial testing.
Do not change monitor arrangement or move the game during a run. Star Rail
rejects geometry drift and foreground loss before sending more input.

Right-click cancellation is polled through the X server, and the GUI Stop
button uses the shared cancellation token. Some Wayland sessions limit global
pointer/button visibility outside XWayland clients; keep the pointer over the
game and retain access to the GUI Stop button.

## Verification and behavior review

Reviewed against main `9b54bacda0222ce7b9d15dbbb6b2500578e57282`, with
`36b82fb` window-enumeration isolation, `8b078bb` GDI cleanup/full-read checks,
and `9b54bac` stop-on-failure behavior retained.

The PR's generic backpack tab OCR retries, extra scroll waits, panel retries and
post-scroll baseline changes were removed from this port. Shared artifact,
weapon, backpack and worker implementations remain identical to main.
`--stop-on-failure` stops at the first OCR failure when enabled; otherwise it
logs, skips that item and continues. There is no ten-error threshold. Windows
click-at extraction preserves move -> 10 ms wait -> existing click behavior.

Linux changes affecting operation are explicit: X11/uinput platform input,
focus/liveness handling, early input permission failure, verified pointer
movement, correct wheel directions, and Star Rail focus/geometry validation.
HDR is rejected and marked unavailable in the UI. The same scan/export/result
policies are used on both platforms.

Tests run on Windows x86_64 MSVC and Ubuntu 24.04 WSL2, pinned Rust 1.95.0:

| Regression group | Windows | Linux |
| --- | --- | --- |
| Core platform unit tests | 9 passed + concurrent enumeration test | 7 passed + isolated X11 fixture |
| Genshin core/API/runtime | 255 passed | 255 passed (includes archive regression) |
| Star Rail core | 97 passed | 97 passed + isolated native-device fixture |
| Application unit tests | 25 passed | 17 passed |
| GUI appearance/layout/feedback/shared/errors | 69 passed | 65 passed |
| Windows updater restart | 4 passed, 1 subprocess helper ignored by direct runner | Windows-only |


- Both Windows edition third-party dependency package/features sets compared exactly to a
  clean main checkout (the checker ignores only the retired local capture-switch names). Linux-specific dependencies do not enter either set;
  pktmon does not enter either Linux edition tree. Reproduce with
  `python scripts/check_platform_dependencies.py --baseline-root <main-checkout>`.
- Windows capture build and both-platform all-target checks, plus scanner,
  GUI, error and updater regression tests.
- Linux Genshin core/API tests, Star Rail core tests, application unit/GUI tests,
  and isolated Xvfb tests through production capture/input/device APIs.
- Real Linux GUI screenshots for both games, CJK text and light/dark palettes.
- Actual ONNX Runtime archive download/extraction and v4/v5 model inference on a
  checked-in image. This proves loading/inference, not game OCR accuracy.

Useful platform commands:

```sh
cargo test -p yas_core --features ort --lib
xvfb-run -a cargo test -p yas_core --test linux_x11 -- --ignored
xvfb-run -a cargo test -p hsr_scanner --test linux_device -- --ignored
cargo test -p genshin_scanner --lib
cargo test -p hsr_scanner --lib
cargo test -p good_tools_app --features test-as-invoker --lib
cargo test -p good_tools_app --features test-as-invoker \
  --test appearance --test gui_layout --test gui_feedback --test shared_games
```

No real Genshin/Star Rail Linux game session was available. KWin/Mutter device
delivery, portal live theme switching, fractional/multi-monitor Wayland input,
multi-page game scanning and actual manager mutations still require operator
testing. WSL/Xvfb checks must not be described as game compatibility validation.

## Shared platform boundary and retired options

OS implementation selection uses Rust's built-in compilation target (`target_os`),
not independently selectable Windows/Linux Cargo features. `yas::window::NativeWindow`
wraps the native handle without owning/destroying the game window. Private OS
backends implement geometry, discovery, activation and foreground queries. Activation
returns `Granted`, `Denied` or `Pending`; shared game code verifies pending compositor
requests before continuing. Game-specific title/class checks remain game policy.

Both games call `yas::capture::create_capturer` and
`yas::system_control::prepared_control`; Windows retains BitBlt/WGC and its existing
input timings, while Linux uses X11/XWayland and input permission preflight. Pointer
drag sequencing and easing also share one implementation; existing backend pacing
is passed as data, and failed movement always attempts button release. Shared
controllers own navigation/retries, and both OS builds use the same ONNX Runtime
preprocessing/inference/CTC decoder and model-selection settings.

Removed unused paths/options:

- `capturer_wgc`, `capturer_screenshots`: native backends are chosen by target.
- `capturer_libwayshot`, its unused backend, unused `StreamingCapturer` and the
  `GenericCapturer` alias whose constructors differed between operating systems.
- `tract_onnx` and the alternate inference implementation unused by either edition.
- Genshin's persisted/core `capture_method`, which execution already derived from HDR.
- Persisted Genshin `save_on_cancel`, Star Rail `scanSaveOnCancel` and GUI state/preview
  wiring. User scans already save partial results; manager jobs retain their separate
  complete-result policy. Old JSON keys are accepted as unknown fields and disappear
  on the next save, preserving other settings.

Live CLI diagnostics, Star Rail's explicit diagnostic capture override, `capture`,
`dev-tools`, `test-as-invoker` and `ort` remain supported. A missing GUI control alone
is not grounds to delete a working CLI or internal manager policy.
