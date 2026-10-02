# Moonboot Testing Plan

## Scope

This supplements IMPLEMENTATION_SPEC.md. Implement both documents together.
The project is not yet implemented; commands and scenarios below are requirements,
not claims of existing functionality or completed tests.

Separate backend correctness, UI correctness, Nix packaging, actual desktop
integration, and real hardware behavior. Passing one layer does not prove another.
Automated tests and desktop demo tests must never control the real plug.
The client is NixOS/Hyprland with Waybar; verify its tray module is enabled.

## Testable Design

- Keep CLI and GUI on the same backend workflow and typed state/event model.
- Isolate Tuya transport, host readiness/Moonlight process execution, clock/sleep,
  and notification delivery behind small test seams, not a large plugin framework.
- Exercise actual signing and response parsing against a fake transport or local
  mock server. Mock only external boundaries, not the workflow under test.
- Inject a controllable clock for fast deterministic polling/deadline tests.
  Test real child-process timeout/termination separately with a fake executable.
- Record commands/events in test fixtures so assertions can prove that forbidden
  actions did not occur, not merely that the expected UI text appeared.
- Use temporary config, credentials, state, and runtime directories. Use synthetic
  credentials and isolated locks; never read the user's normal config in tests.
- Restrict test-only HTTP overrides to test transport setup. Do not weaken the
  production HTTPS requirement to make a localhost mock server convenient.

## Hardware-Isolated Demo Mode

Provide these entry points, or document equivalent explicit CLI syntax:

```sh
nix run . -- gui --demo success
nix run . -- gui --demo cloud-error
nix run . -- gui --demo readiness-timeout
nix run . -- tray --demo success
```

- Demo startup must select fake dependencies before reading production config.
  It needs no Tuya credentials, host address, Sunshine, or Moonlight installation.
- Never read real secrets, authenticate to Tuya, probe the host, send real power
  commands, or spawn real Moonlight, even if a production config is present.
- Local session D-Bus is allowed for tray/notifications; no outbound Tuya/host
  networking is allowed. Assert isolation with fake-call recording and tests.
- Give demo its own UI instance identity, operation lock, log/state directory,
  and IPC namespace so it cannot control or collide with a production instance.
- Display DEMO prominently in the panel and tray tooltip/menu. Demo notification
  content must also identify it as simulated.
- Use synthetic host/device labels. Fake boot progression should take a few
  seconds, while deterministic unit tests advance their clock without waiting.
- Simulate streaming visibly without opening a real stream; provide a way to end
  that simulated session so manual plug-off testing is reachable.
- Support success, already-on, cloud-error, readiness-timeout, and off-error
  scenarios. These cover normal UI, cancellation, error presentation, and an
  unconfirmed off result. Scenario behavior must be documented and repeatable.
- Keep typed off confirmation in demo too; do not teach a shortcut that bypasses
  the production safety flow. Restarting demo resets simulated state.
- Never promote a running demo to live mode. Switching requires explicitly
  launching a separate production invocation.

## Layer 1: Backend Tests

| Area | Cases and required assertions |
| --- | --- |
| Signing | Known public-cloud fixtures; token/business differences; query canonicalization; uppercase HMAC; exact sent body hash |
| Config/secrets | Missing/invalid config, unsafe credentials permissions, non-HTTPS endpoint, invalid timing values; no secret logging |
| Cloud responses | HTTP failure, JSON failure within HTTP 200, malformed JSON, expired token, rate limit, unsupported switch, offline device |
| Cold start | Explicit on, reported-on confirmation, repeated readiness probes, exactly one streaming launch |
| Already on | No power command; readiness and launch still work |
| Ambiguous on | Query state before any bounded retry; never toggle |
| Deadlines | Readiness timeout, hung probe, remaining-budget clamping, bounded status polling; terminate and reap subprocesses |
| Cancellation | Stop waits/processes cleanly; no off command at any point |
| CLI execution | Preserve spaces in app names/arguments; no shell expansion or injection; document exit codes |
| Concurrency | Shared CLI/GUI operation lock; second action sends no command; release lock after exit/cancellation |
| Plug off | Fresh approval required; rejection, EOF, piped input, dialog close, and cancel send nothing; already-off sends nothing |
| Ambiguous off | Query status, report uncertainty if unconfirmed; no blind resend or toggle |
| Notifications | Missing/failing daemon cannot break the workflow |
| Idle/demo | No cloud calls, credential reads, host probes, real Moonlight, or power changes on idle startup; demo always isolated |

Test terminal confirmation with a pseudo-terminal where necessary. A piped phrase
must not satisfy the CLI's interactive-input requirement. Do not add production
approval-bypass flags to make tests easier.

## Layer 2: Automated UI Tests

Use egui_kittest with a version compatible with the pinned egui/eframe release.
Prefer label/accessibility queries to fragile pixel coordinates. Test a panel
against the fake backend with deterministic events:

- Start Session triggers exactly one workflow and displays successive states.
- Progress/errors appear while controls remain responsive.
- Start/Plug Off are disabled during conflicting operations and streaming.
- Cancel Startup ends waiting but never emits off.
- Plug-on status is not described as proof the PC is running.
- Unknown/stale status and observation timestamps are represented honestly.
- Off dialog shows host/device and warning, starts empty, and requires the exact
  phrase plus a distinct confirmation click.
- Wrong phrase, Cancel, and dialog close emit no off request.
- Approval is not retained across dialogs, failures, or sessions.
- Hiding the panel preserves the active workflow; simulated completion updates
  state correctly when reopened.
- Demo labeling is always visible and simulated results cannot be mistaken for live.

Screenshot tests are optional initially. If added, fix dimensions, fonts, scaling,
and renderer; keep baselines reviewed and separate GPU-dependent checks from
mandatory headless tests. UI harness tests do not prove tray or Wayland lifecycle.

## Layer 3: Nix And CI

Document and run:

```sh
nix develop
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
nix build
nix flake check
```

- Flake checks must expose hardware-free Rust tests and quality checks. Dependency
  fetching during Nix setup/build is distinct from tests contacting a live service.
- Checks run without production credentials, Sunshine, a desktop session, or the
  real plug. D-Bus protocol tests may run under an isolated dbus-run-session.
- Verify package wrappers resolve Moonlight and notification executables and the
  GUI's graphics dependencies on NixOS, without relying on a development shell.
- Test the built package's help/version and demo launch, not only `cargo run`.
- Verify credentials are excluded from Nix sources/store outputs. Do not use real
  secrets to test exclusion; use synthetic sentinel files and explicit source filters.
- CI must never enable live tests by default or require secret credentials.
- Separate optional rendering tests if the chosen renderer needs GPU/software
  rendering infrastructure not available in the basic build sandbox.

## Layer 4: Hyprland And Waybar

Perform these checks in the user's actual graphical session using demo mode first.
Obtain approval before restarting Waybar or changing desktop configuration.

1. Start demo tray: one icon appears, with no panel or startup notification.
2. Open its menu and panel; verify DEMO labeling and controls.
3. Hide and reopen it repeatedly; no duplicate controllers/windows appear.
4. Start a fake session; verify progress and responsiveness, including while hidden.
5. Cancel during fake startup; verify no simulated off and no real side effects.
6. End the fake stream, then exercise typed off approval and rejection.
7. Run cloud-error/readiness-timeout/off-error demos and inspect messages/logs.
8. Verify notifications with the existing daemon; verify graceful behavior without
   notification delivery using a fake notifier rather than stopping user services.
9. Restart Waybar with approval; check icon re-registration after tray-host recovery.
10. Exercise missing-tray-host behavior through isolated integration tests or an
    approved session change; confirm diagnostics and `gui` fallback.
11. Check readable layout, keyboard navigation, focus, and the user's display scale.
12. Verify optional GUI shortcut and tray autostart only after approval to install
    their configuration. At the next login, exactly one idle controller should run.

For a production idle-start check, use call instrumentation or backend assertions
to verify no cloud/host access. Starting production tray must be inert by design;
never click a live action as part of a demo check.

## Layer 5: Controlled Live Validation

Live commands require explicit user approval. Ensure the user can reach the host
or otherwise confirm shutdown independently. Do not infer shutdown from ping,
Moonlight disconnection, a closed port, or a fixed delay.

1. Confirm regional endpoint, plug ID/code, private credentials, pairing under the
   client user, host address, application name, and initial plug/host state.
2. Run the read-only `check` command and compare switch status with the app.
3. With the host already running, test Start Session; confirm no power toggle.
4. Close Moonlight normally; verify plug stays on and host is not shut down.
5. Shut Linux down gracefully and verify completion independently. Test rejected
   off confirmation first; verify plug stays on. Then approve Plug Off and verify
   reported/physical off. An off test must never target a running host.
6. From this shut-down/off state, approve a cold start; verify AC-restoration boot,
   Sunshine readiness, and exactly one real Moonlight session.
7. If testing live startup cancellation, understand it may leave the host booting
   or running. Cancellation leaves power on; do not follow it with immediate off.

Use mocks for dangerous/rare failures such as ambiguous off, network loss during
power operations, invalid signatures, and hung processes. Do not manufacture these
by cutting power to the real running PC or disrupting the user's network.

## Verification Report

The implementing agent must report commands run, pass/fail results, skipped checks
and reasons, and desktop/hardware checks still pending. Record app/dependency
versions relevant to tray/Wayland behavior. Never include credentials in reports.

Completion requires mandatory hardware-free checks passing plus a documented demo
mode. If no graphical session or live-test approval is available, report desktop
or live validation as pending; do not claim mocks prove real hardware success.

## Reference

- egui testing harness:
  https://github.com/emilk/egui/tree/main/crates/egui_kittest
