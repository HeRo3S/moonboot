# Verification Report

Recorded on 2026-10-02. All device behavior used synthetic fixtures or explicit
demo mode. **No live Tuya authentication, plug command, host readiness probe or
real Moonlight session was performed by this implementation session.** Existing
desktop applications were not used as test targets.

## Environment And Versions

| Item | Verified version/selection |
| --- | --- |
| Target | x86_64-linux, NixOS, logged-in Wayland desktop |
| Nix | 2.34.7 |
| Pinned Nixpkgs | `c59305bab2065cfecc4944690d9eedbb56f3a9fa` |
| Development-shell Rust | rustc 1.98.1, cargo 1.98.0 |
| eframe / egui / egui_kittest | 0.33.3 |
| Native renderer | glow with Wayland |
| Winit | 0.30.13 |
| ksni | 0.3.6, blocking API |
| Reqwest | 0.12.28, rustls, asynchronous cancellable transport |
| Packaged Moonlight Qt | 6.1.0; executable verified as `moonlight` |
| Actual desktop Hyprland | 0.55.2 |
| Actual desktop Waybar | 0.15.0 |
| Actual notification daemon | Dunst 1.13.2 |
| Actual desktop scale | 1.0 on 1920x1080 displays |

The user's Waybar configuration includes `tray` in its enabled modules, and the
active session D-Bus has a StatusNotifierWatcher. No bar restart, configuration
edit, Home Manager activation or NixOS rebuild was performed.

## Mandatory Hardware-Free Checks

| Command/check | Final result |
| --- | --- |
| `nix develop` | Development shell realized and used for Rust commands |
| `cargo fmt --check` | Passed |
| `cargo clippy --all-targets -- -D warnings` | Passed |
| `cargo test` | Passed: 52 library tests and 12 integration tests |
| `nix build --cores 6` | Passed; produces wrapped executable in `result/bin/moonboot` |
| `nix flake check --keep-going --cores 6` | Passed: quality, package, source-safety and package-smoke |
| Packaged `--help` / `--version` | Passed |
| Packaged idle demo tray with minimal PATH on isolated D-Bus | Passed in flake check |
| `git diff --check` | Passed |

Flake checks compile/run offline against vendored Cargo dependencies, with
temporary HOME/XDG directories and no production credentials, desktop display or
real session bus. Dependency retrieval/build substitution occurs before those
checks and is not described as an air-gapped build.

Nix emits an informational unknown-output warning for `homeManagerModules`, a
conventional extension output. It does not fail evaluation or checks.

Earlier attempts exposed and corrected new-Clippy byte-string warnings and a
missing default D-Bus config inside the sandbox. The isolated smoke check now
supplies its own D-Bus configuration. An initial two-core build exceeded the tool
time budget; the subsequent six-core builds completed. These earlier attempts are
not reported as successful checks.

## Automated Coverage

- Current public Tuya token/business signing fixtures, query ordering, uppercase
  HMAC and hashing the exact body sent.
- Signed fake transport through actual cloud parsing and workflow, not a mocked
  replacement for the entire cloud/control path.
- HTTP/JSON failures, false command results, malformed/missing results, offline
  device, unsupported/nonboolean/duplicate controls, and read-only token renewal.
- Unsafe/missing credentials and config, HTTPS-only production transport,
  redirect refusal, response bounds and fixed/redacted diagnostics.
- Cold start, already-on, read-only checks, permanent app mismatch, ambiguous
  on/off reads without resends, status confirmation failure and no forbidden off.
- Injected clock deadlines, budget clamping, cancellation during waiting,
  cancelled/stalled HTTP headers/body, and timeout across both headers and body.
- Real fake-process timeout, kill/reap, argument preservation without a shell,
  and best-effort missing/failing/hung notification delivery.
- Forced supervisor death: process-group guardian cleanup, lock release,
  descriptor closure, restored signal masks, failed exec, and unrelated process
  preservation. The isolated regression was also repeated five times.
- Private locks/logs/IPC, symlink refusal, shared operation exclusion, CLI approval
  rejection before config reads, exact PTY input/EOF/interrupt handling.
- Hardware-isolated demo dependency selection before config loading, synthetic
  file open observers, distinct live/demo identities, and fake-command recording.
- egui label/accessibility interaction tests for progress, conflicting controls,
  typed phrase plus separate confirmation, rejection/close/Escape, fresh approval
  including undo-history isolation, hiding and delayed hidden confirmation.
- Headless lifecycle regressions for session preservation across panel destruction,
  Hide/Open/Close-event races, Quit during initialization and bounded child cleanup.
- Isolated D-Bus inert tray startup and clean termination, without a real desktop.
- Missing-watcher diagnostics, same-PID tray recovery after watcher appearance,
  disconnect and replacement, exact DEMO menu/identity, no secret/config reads,
  no UI/power operation, and Quit over the isolated D-Bus protocol. This recovery
  regression was also repeated five times.
- Successful notifier execution from a worker through absolute paths and PATH,
  preserving literal message arguments. Packaged notifier smoke reaches a private
  bus with a deliberately incompatible libnotify in the inherited library path.
- Nix source-filter assertions reject synthetic secret paths and symlinks; the
  actual source output is checked for excluded config/credential/artifact paths.

## Native Desktop Demo Checks

The wrapped Nix package was exercised on the actual Hyprland/Waybar session,
outside `nix develop`. Tests targeted only exact `demo-SCENARIO-moonboot` native
windows and the owned demo tray's D-Bus identity. Keyboard events were addressed
to the guarded window, not sent to a real Moonlight window. Cropped panel images
were inspected; screenshots and one-off driver files remain outside the repo in
`/tmp/opencode` and contain no credentials.

Verified with the packaged demo:

- Idle tray registers and publishes DEMO tooltip/menu, without opening a panel.
- Before/after cropped Waybar images show the bundled crescent icon appearing
  once; no Waybar restart or configuration change was needed.
- Menu exposes Open DEMO Controls and Quit, with no one-click power action.
- D-Bus menu Open creates one native Wayland panel with prominent DEMO labeling.
- Start Session displays simulated progress and disables conflicting controls.
- `already-on` proceeds from reported on to a simulated stream without simulated
  switching-on; its end/rejection/approval path was also exercised natively.
- WM Close removes the native panel while retaining the controller process.
- Reopen recreates one panel on the same controller; a simulated stream survives
  the hidden interval. Repeated GUI requests do not create duplicates.
- End Demo Session returns to idle with the simulated plug still on.
- Plug Off starts with an empty phrase, requires exact `POWER OFF`, and needs a
  separate confirmation activation. Successful simulated off displays reported off.
- Escape rejection leaves simulated power on; a new dialog starts fresh.
- `off-error` displays an unconfirmed result and retains the last on observation.
- `cloud-error` displays a simulated cloud failure with Unknown status.
- `readiness-timeout` displays an actionable error and preserves simulated on.
- Cancel during simulated readiness stops waiting without reversing power.
- Tray Quit closes/reaps its panel controller, including a simulated stream.
- Error-state demo logs are private/bounded by implementation and automated tests.
- DEMO failure notification reaches the existing Dunst daemon and visibly shows
  the simulated-only warning. A filtered notification D-Bus trace and cropped
  notification image were inspected; the daemon was not stopped or reconfigured.

The native tests detected that Winit ignores `set_visible` on Wayland. The
implementation was corrected to retain the controller/session but destroy and
recreate only native windows. Native hide/reopen was then retested. A subsequent
close/Open race and forced-cleanup issue have dedicated automated regressions.
The final built package's success and already-on desktop flows were retested
after those changes.

The follow-up desktop audit found an inherited-library packaging collision:
OpenChamber's `LD_LIBRARY_PATH` selected another `libnotify.so.4`, and notify-send
exited 127 before contacting D-Bus because GDK Pixbuf could not be loaded. The
wrapper now prefers its packaged libnotify ahead of inherited directories while
preserving the inherited path. A synthetic incompatible-library smoke check and
real DEMO notification delivery passed after the fix.

Parallel build load also exposed two fixture timing assumptions: the hung-process
test allowed only 80 ms for cold exec before expecting emitted PIDs, and the
missing-watcher test sampled a legitimate failed optional-notifier fork. The
fixtures now allow a one-second exec budget and bounded notifier reaping,
respectively; their process-cleanup assertions remain intact and repeat runs pass.

## Home Manager Export Evaluation

The actual `homeManagerModules.default` export, not a stand-in snippet, was
evaluated with Home Manager 26.11 at revision
`833540099ef43cbeb28b1e3f3c21901961edb48e` and the project's locked Nixpkgs.
All 12 positive configurations passed: disabled, package-only, autostart-only,
shortcut-only and both integrations, including hyprlang and Lua selections.
Both integration-without-Hyprland cases failed with the intended assertion.

Baseline comparisons confirmed the actual Moonboot package export, preserved
existing compositor configuration, idle `tray` autostart and `gui` shortcut only,
and no added power commands, credentials, services, portals or session environment
changes. The generated Lua calls were checked against current official Hyprland
documentation. This was evaluation only, not activation or a next-login test.

## Remaining Validation

- Other display scales and an actual Waybar restart have not been exhaustively
  checked. The current icon placement and isolated watcher-protocol recovery were
  verified; no permission to restart Waybar was assumed.
- Autostart/shortcut snippets were not installed. Actual next-login behavior and
  user-chosen shortcut require opt-in configuration and a later login.
- The actual exported Home Manager module was evaluated as described above;
  the user's system/module configuration was not activated or altered.
- Real pairing/app-list output from a configured host remains unverified. Parsing
  was checked against upstream Moonlight source and fixtures, and the packaged
  client version/executable were checked locally.
- No controlled live checks were run. Private endpoint/device/code, runtime
  credentials, host/app, paired profile, initial state and explicit authorization
  are still needed. An off test additionally requires independently completed
  Linux shutdown and the same fresh interactive confirmation as normal operation.

Mandatory hardware-free checks and documented demo mode are complete. Mocks and
native simulated desktop sessions do **not** establish real plug, BIOS,
Sunshine or streaming success.
