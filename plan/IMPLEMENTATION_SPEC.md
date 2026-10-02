# Tuya-to-Moonlight Launcher

## Agent Handoff

Implement this project from scratch in the project directory selected by the user.
This document is a specification, not a request to modify the user's machine
configuration automatically. Inspect the target directory before making changes.
Preserve existing files and unrelated changes. Do not commit unless asked.
Read the companion [TESTING_PLAN.md](./TESTING_PLAN.md) before implementation.
Its demo isolation, test layers, and verification requirements are part of scope.

The user selected Tuya cloud control, an all-Rust egui panel opened from a tray
icon, and silent tray startup at Hyprland login. Start Session powers on, waits
for Sunshine, and launches Moonlight. A separate manually confirmed plug-off
action is available after host shutdown. Retain CLI commands for terminal use.
Use stable Rust. Qt and Python are not required. Do not introduce nightly without
a demonstrated dependency requirement and explicit user approval.

## Goal And Environment

Provide one command that turns on a Tuya smart plug, waits for a Linux streaming
host to boot and expose Sunshine, and starts a Moonlight session on the client.
Provide a separate command to cut plug power only after the user confirms that
the host has finished shutting down. This confirmation is a human safety decision,
not automatic proof of the host's power state.

- Client: NixOS with Hyprland and a logged-in Wayland graphical session.
- A StatusNotifierItem tray host, such as Waybar with its tray module enabled,
  is required to display the icon. Hyprland itself does not provide a tray.
  The user has confirmed they use Waybar; verify that its tray module is enabled.
- Host: Linux PC connected to the plug, BIOS configured to power on when AC is
  restored, autologin enabled, Sunshine started automatically.
- Tuya developer project and app-account linking have reportedly been prepared.
- Moonlight runs on the client, not on the initially powered-off host.
- Host and client must have usable network connectivity, directly or through an
  existing VPN. Do not expose Sunshine or add port-forwarding automatically.

## Scope And Safety

- `start` must never send an off command, reset the plug, or power-cycle it.
- Only the separate `plug-off` command or GUI Plug Off action may send off, after
  explicit interactive confirmation. Never infer shutdown from a failed ping, closed port, elapsed
  time, or disconnected stream. Never turn off automatically after stream exit.
- If the plug is already on, do not toggle it. Try host readiness normally.
- An already-on plug with an off PC cannot trigger AC-restoration boot. On timeout,
  explain this possibility without assuming it is the cause or cutting power.
- API success means the command was accepted, not that Linux or Sunshine is ready.
- On failure, cancellation, or stream exit, leave the plug's power state alone.
- Login starts only the idle tray controller: no power action, Moonlight launch,
  Tuya authentication, or cloud polling.
- No automatic shutdown, Wake-on-LAN, initial pairing, cloud provisioning, system
  daemon, or modification of BIOS, Sunshine, firewall, or Hyprland.
- Never log credentials, tokens, signed headers, or full sensitive API responses.

## Proposed Stack

Use stable Rust with Cargo. Prefer a small dependency set and synchronous control
flow unless asynchronous execution materially simplifies subprocess cancellation.
Suggested crates: clap, serde, toml, reqwest with rustls, hmac, sha2, and a small
advisory-lock/signal-handling dependency if needed. These are suggestions, not a
requirement to add an abstraction framework. Use current documentation to confirm
dependency APIs and Tuya request signing before implementing.

Use egui/eframe with Wayland support for the panel and a Linux StatusNotifierItem
integration such as ksni for the tray. Verify current APIs, stable Rust minimum
versions, D-Bus behavior, and Nix dependencies first. egui draws its own controls;
matching Qt widget appearance is not required. GUI network/subprocess work must
run outside the UI event loop, even if the CLI backend uses blocking operations.

Use Moonlight Qt as an external executable. Native Nix packaging is the initial
target, not Flatpak. Discover the executable name from the selected Nixpkgs package;
do not assume it is identical across installation methods.

## CLI Contract

Working name: `moonboot`.

```sh
moonboot start
moonboot start --config /path/to/config.toml
moonboot check
moonboot plug-off
moonboot tray
moonboot gui
```

- `start`: full workflow, then supervise Moonlight until its process exits.
- `check`: validate configuration, dependencies, credentials, cloud connectivity,
  plug status and supported switch function without changing power or starting a
  stream. An offline PC is not a failure for this command.
- `plug-off`: interactively confirm completed host shutdown, then send an explicit
  off command and verify reported switch status. See the workflow below.
- `tray`: run the idle session tray controller without opening a window, contacting
  Tuya, changing power, or launching Moonlight. Open the panel on user request.
- `gui`: open the panel on demand. Repeated requests should reuse an existing
  panel/controller instead of creating duplicate windows.
- Provide `--help`, `--version`, and optional verbose diagnostic output with redaction.
- Progress goes to the terminal; concise failures go to stderr.
- Give stable documented exit codes for configuration/dependency failure, cloud
  failure, readiness timeout, already-running invocation, and stream failure.
- Handle Ctrl+C and termination cleanly, including an in-progress readiness probe.
  If Moonlight has been launched, forward termination appropriately and reap it.
- Do not build commands as shell strings; pass each argument separately.

## Configuration And Secrets

Default config: `$XDG_CONFIG_HOME/moonboot/config.toml`, falling back to
`~/.config/moonboot/config.toml`.

Provide an example containing placeholders, not live credentials:

```toml
[tuya]
endpoint = "https://REPLACE_WITH_YOUR_TUYA_REGIONAL_ENDPOINT"
device_id = "REPLACE_WITH_PLUG_DEVICE_ID"
switch_code = "REPLACE_WITH_SUPPORTED_BOOLEAN_SWITCH_CODE"
credentials_file = "/absolute/path/to/private-tuya-credentials.toml"

[moonlight]
executable = "moonlight"
host = "REPLACE_WITH_HOST_IP_OR_NAME"
app = "Desktop"
stream_args = []

[startup]
timeout_seconds = 180
poll_interval_seconds = 3
probe_timeout_seconds = 10
http_timeout_seconds = 10

[notifications]
enabled = true
```

The private credentials file contains `client_id` and `client_secret` and should
be owned by the user with mode 0600. Do not silently change permissions; report
unsafe permissions clearly. Resolve any supported home-directory expansion
explicitly; do not assume Rust expands `~` automatically.

Never put credentials in the repository, command-line arguments, flake outputs,
Nix derivation environment, or Nix/Home Manager file contents. Nix store files
are generally readable by other users. Examples must reference a runtime secret
file outside the repository/store. Existing sops-nix or agenix integration can be
documented as optional, but must not become a prerequisite.

Validate positive timeout values and a valid HTTPS Tuya endpoint. Preserve TLS
certificate verification. Do not follow redirects that could forward credentials
to another origin. Treat configurable executable arguments as data, not shell code.

## Startup Workflow

1. Read and validate config. Verify dependencies before changing plug state.
2. Acquire a per-user advisory lock under `$XDG_RUNTIME_DIR`. A second invocation
   must fail clearly, without sending another command. Share this lock with
   `plug-off`. Hold it through the streaming session; release it on termination.
   An idle tray/panel must not hold this operation lock. Use a separate UI
   single-instance mechanism.
3. Authenticate against the configured regional Tuya endpoint using the project
   Access ID/Secret. Keep tokens in memory only.
4. Read plug status and verify that the configured switch code is a supported
   boolean control. An unknown code or offline plug should produce an actionable
   failure, not an invented default or successful status.
5. If reported off, send an explicit on command. If reported on, skip the command.
6. For a sent command, check both HTTP status and Tuya's JSON success/result fields.
   Poll switch status with a bounded wait, for example 20 seconds, to confirm on.
   If confirmation fails, report that the power state is unconfirmed and stop.
7. Poll Moonlight/Sunshine readiness immediately, without a mandatory 60-second
   delay. Use `moonlight list HOST` as the preferred end-to-end probe. Verify the
   installed client's behavior and require a successful result with the configured
   app present, not merely that a TCP connection or ping succeeds.
8. Terminate each hung probe at its timeout and reap its subprocess. Use a monotonic
   overall readiness deadline. Clamp probe and sleep durations to the remaining
   deadline so a 180-second deadline cannot stretch indefinitely.
9. Distinguish permanent failures, such as missing pairing or app name mismatch,
   from transient host-not-ready failures where the CLI output permits reliable
   classification. Avoid fragile parsing of localized human-readable error text.
10. Once ready, launch `moonlight stream HOST APP` with configured extra stream
    arguments. Wait for its exit and report a failed launch/stream appropriately.

Moonlight app-list output format must be verified against the installed version.
Keep parsing isolated and covered by fixtures. If reliable classification or app
matching cannot be implemented, document the limitation explicitly rather than
treating any output as readiness. Initial pairing must already be completed for
the same client user/profile running the launcher.

Do not retry a command by toggling power. Bounded retries of an explicit `on` are
safe in principle, but after an ambiguous network result first re-query status.
Respect rate-limit responses and do not create unbounded cloud retry loops.

## Manually Confirmed Plug-Off

This is not an OS shutdown command. The user shuts down the host through Linux
first, waits until shutdown completes, then runs `moonboot plug-off` on the client.
Ending Moonlight streaming alone does not shut down the host.

1. Validate the Tuya configuration and acquire the same lock as `start`. Refuse
   while another launcher invocation or its supervised stream is active.
2. Authenticate, validate the configured switch, and query device status. If
   already reported off, say so and exit without issuing a power command.
3. Require interactive terminal input for CLI or an explicit GUI confirmation
   dialog. Show the configured host and plug device
   ID, explain that this cuts AC power, and warn of data loss if Linux is running.
4. Ask the user to confirm that shutdown has fully completed by typing the exact
   phrase `POWER OFF`. GUI requires typing the same phrase plus a confirmation
   click, with no prefilled or remembered approval. Empty input, other text, EOF,
   closing the dialog, or cancellation must send no
   off command. Do not accept a generic `--yes`, environment bypass, piped input,
   or remembered approval. No automatic or noninteractive off option in v1.
5. After confirmation, send the configured switch command with `value: false`.
   Check HTTP and Tuya result fields and confirm reported off with bounded polling.
6. On an ambiguous command result, query status first. Do not blindly resend off:
   cutting and restoring AC could boot the host during a concurrent external
   action. If off is not confirmed, report uncertainty and stop; do not toggle.

Warn users that app control or other automation can change power state outside
this launcher's lock. Manual confirmation cannot prevent all such races and must
not be advertised as automatic shutdown verification. Reject noninteractive
`plug-off` with an actionable message to use a terminal or the GUI. Tray menus and
shortcuts may open the confirmation dialog but must never send off directly.
The off command does not require a working Moonlight installation to operate.

## Tuya API Details

Verify current public-cloud documentation and the project's authorized API products.
Expected endpoints for the standard device API are:

- `GET /v1.0/token?grant_type=1` for authentication.
- `GET /v1.0/devices/{device_id}/functions` for supported controls.
- `GET /v1.0/devices/{device_id}/status` for switch status.
- `POST /v1.0/devices/{device_id}/commands` for an explicit switch command.

```json
{"commands":[{"code":"CONFIGURED_SWITCH_CODE","value":true}]}
```

`start` uses `true`; only the confirmed `plug-off` workflow uses `false`.

Use the current cloud-authorization HMAC-SHA256 signing algorithm. Token requests
and authenticated business requests have different signing inputs. Canonicalize
the path/query correctly, use the required millisecond timestamp, hash the exact
body bytes sent, and produce the required uppercase hexadecimal signature.
Do not implement the obsolete client-ID-plus-timestamp-only signing scheme.

Handle API errors inside HTTP 200 responses. On token expiry, obtain a new token
and retry a safe request once. Explain errors such as invalid credentials, wrong
regional endpoint, expired service entitlement, unauthorized device, clock skew,
offline device, and rate limiting without exposing secrets.

## Hyprland UX

- At login, start `moonboot tray` quietly: no window, splash, startup notification,
  cloud traffic, power action, or Moonlight process in the normal case.
- Provide an optional Home Manager/Hyprland `exec-once` autostart snippet. If
  documenting a user-service alternative, tie it to the actual compositor session
  and imported graphical environment; do not enable both autostart paths.
- Register a StatusNotifierItem on the session D-Bus with a bundled icon. Offer
  Open Controls and Quit in its menu. Opening the panel must work from the menu
  even if tray hosts differ in left-click behavior. No one-click off action.
- Open a compact normal Wayland window, not a layer-shell desktop widget or a
  tray-anchored popup. Do not assume absolute placement/forced focus. Document
  optional Hyprland floating rules.
- Panel controls: Start Session, Cancel Startup while waiting, Plug Off, Refresh
  Status, and Hide. Closing the panel hides it and leaves the tray alive. Quit
  is explicit. No separate power-on-only control in v1.
- Show configured host/app, plug status with timestamp (or Unknown), progress,
  and actionable errors. Plug-on must not be labeled PC-running. Idle means no
  cloud polling; Refresh Status is user-initiated.
- Model Idle, Switching On, Waiting for Sunshine, Launching Moonlight, Streaming,
  Confirming Plug Off, Switching Off, and Error states. Disable conflicting
  controls during operations and streaming, respecting the shared CLI/GUI lock.
- UI and CLI share tested backend logic, not duplicate signing/control paths.
  Workers send state updates to the UI; boot waiting and streaming cannot freeze it.
- Hiding the panel must not cancel startup or terminate streaming. Cancel Startup
  stops local waiting without sending off. Quit/termination may cancel local work
  and the supervised Moonlight client, but never shuts down the host or cuts power.
- Verify tray/window lifecycle on Wayland. Hidden windows may stop redrawing;
  tray handling and workflows must not rely on hidden redraw callbacks. A tray
  controller with an on-demand panel process is acceptable if needed; use secure
  same-user IPC and preserve operation locking and confirmation boundaries.
- If session D-Bus or the tray host is missing, log an actionable diagnostic and
  offer `moonboot gui` as a fallback. Do not claim an icon is visible or force a
  window to appear during otherwise silent login startup.
- Run as the logged-in desktop user with the existing Wayland and session D-Bus
  environment. Do not launch it as root or as a system boot service.
- Use `notify-send` from libnotify for best-effort notifications: starting,
  connecting, and failure. Avoid one notification per poll. Missing notification
  services must not prevent an otherwise working session.
- Document a Hyprland `bind = ..., exec, ...` example using the installed launcher
  path to open `moonboot gui`. Do not edit the user's config. Moonlight launches
  only through Start Session or CLI `start`.
- Ensure shortcut-triggered failures remain visible via a notification and a
  private, bounded log under the user's XDG state directory. Redact secrets there
  too. Do not rely exclusively on an invisible terminal's stderr.
- Do not force graphical variables unless verified necessary; preserve the
  user's working Moonlight desktop environment.

## Nix Deliverables

Provide `flake.nix`, `flake.lock`, `Cargo.toml`, `Cargo.lock`, Rust source, config
example, tests, README, and `.gitignore`. Keep the layout small and understandable.

- Pin Nixpkgs through the flake lock; use `rustPlatform.buildRustPackage` with
  `cargoLock.lockFile = ./Cargo.lock` or another reproducible supported method.
- Provide `packages.<system>.default`, `packages.<system>.moonboot`, and
  `apps.<system>.default` so `nix build` and `nix run . -- start` work.
- Target `x86_64-linux` initially. Add other systems only if their dependencies
  can be evaluated and tested; do not promise unverified architecture support.
- Provide `devShells.<system>.default` and offline hardware-free flake checks for
  the Rust tests/quality checks. `nix flake check` must never control a real device.
- Provide a development shell with Rust tooling, Moonlight Qt, and libnotify.
- Include the selected egui renderer's build/runtime dependencies, Wayland,
  input libraries, and tray D-Bus requirements. Verify graphics library resolution
  on NixOS rather than assuming CLI packaging covers the GUI runtime.
- Provide optional Home Manager tray autostart and panel shortcut configuration,
  documenting the Waybar tray prerequisite without changing the user's bar config.
- Ensure the packaged launcher can resolve Moonlight and notify-send without
  depending on an interactive shell's PATH, for example with a package wrapper.
- Exclude secrets and generated artifacts from package source inputs.
- Document declarative installation and an optional Home Manager integration.
- The README must show adding this project as a system-flake input and installing
  `inputs.moonboot.packages.${pkgs.stdenv.hostPlatform.system}.default` through
  `environment.systemPackages` or Home Manager's `home.packages`. Explain passing
  the input to the module with `specialArgs`/`extraSpecialArgs` as appropriate.
- Document `nix develop`, `nix build`, `nix flake check`, `nix run . -- check`,
  `nix run . -- start`, interactive `nix run . -- plug-off`, `nix run . -- tray`,
  and `nix run . -- gui`.
- Keep credentials runtime-only even when configuration is declarative. Provide
  no service that runs plug-off at logout, shutdown, or Moonlight exit.
- Do not change the user's system flake or run `nixos-rebuild` without approval.

## Tests And Acceptance

Implement the detailed [TESTING_PLAN.md](./TESTING_PLAN.md), including a visibly
labeled, hardware-isolated demo mode for panel/tray testing. Demo mode must never
read real credentials, contact Tuya/the host, or launch the real Moonlight process.
Use egui_kittest for control interaction/state tests where supported by the pinned
egui version. Keep real desktop checks distinct from headless UI tests.

Automated tests must not control the user's real plug. Use a fake HTTP transport
or mock server and a fake Moonlight executable, with injectable clock/sleep where
needed to keep deadline tests fast.

Cover signing fixtures, exact request bodies, JSON API failures, missing/unsafe
credentials, supported switch validation, already-on behavior, offline plug,
ambiguous on-command responses, token renewal, bounded timeout, hung probes,
pairing/app failures, cancellation, concurrent invocations, notifications failing,
and Moonlight arguments containing spaces without shell injection.
Also cover plug-off approval, rejection, EOF, noninteractive input, already-off
status, shared-lock refusal, ambiguous off responses, status-confirmation failure,
and the invariant that `start` and `check` can never issue an off command.
Also test UI state transitions, dialog rejection/close, conflicting actions,
cancellation, and the absence of cloud/power/stream actions at tray startup.
GUI approval must preserve the backend off authorization boundary; IPC must not
allow an unconfirmed power-off request. Hardware-free tray tests may use an
isolated D-Bus session.

Acceptance criteria:

- `start` emits only on commands; `check` emits no power commands. `plug-off`
  emits off only after the required interactive confirmation.
- A successful mocked cold-start workflow launches the requested stream once.
- An already-on plug is not toggled and can connect to an already-running host.
- Startup failure and timeout paths do not launch Moonlight streaming or cut power.
- Plug-off cancellation/refusal sends no off command; successful confirmed
  plug-off verifies reported off without shutting down or restarting the host.
- A shortcut launches Moonlight in the user's Hyprland session and reports failure
  visibly after the user chooses Start Session in the panel, assuming a notification
  daemon is installed and running.
- Login creates only an idle tray icon, with no cloud calls, power commands, or
  Moonlight process. With a tray host available, the panel opens/hides/reopens,
  shows progress, and cancels startup without freezing.
- Both terminal and GUI off actions require fresh typed confirmation. Closing
  a panel or Moonlight cannot turn off the plug.
- `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`,
  `nix build`, and `nix flake check` pass where tooling/network are available.
  Report any unrun checks.

Live validation requires explicit user approval because it can change PC power.
An off-command test additionally requires completed host shutdown and the same
interactive confirmation as normal operation. Before a live startup run, confirm pairing,
host address, app name, and the plug's initial state. A real stream/desktop test
must be performed in a graphical client session, not claimed from mocks alone.

## Remaining User Inputs

These are setup values, not reasons to block writing and testing the project:

- Tuya regional API endpoint, device ID, and exact supported switch code.
- Client ID/Secret entered privately into the runtime credentials file.
- Host IP/name and exact Sunshine application name.
- Confirmation of Moonlight pairing under the client desktop user.
- Optional stream settings and preferred Hyprland shortcut.
- Waybar is installed; confirm its tray module is enabled before desktop testing.

Default to native Moonlight Qt, a 180-second readiness timeout, and no additional
stream flags. Ask only if an actual implementation tradeoff requires a decision;
do not invent credentials, device codes, host addresses, or user preferences.

## References

- Tuya project and linking guide:
  https://developer.tuya.com/en/docs/iot/Platform_Configuration_smarthome?id=Kamcgamwoevrx
- Tuya cloud authorization signing:
  https://developer.tuya.com/en/docs/iot/singnature?id=Kbw0q34cs2e5g
- Moonlight Qt source and CLI definitions:
  https://github.com/moonlight-stream/moonlight-qt
- Nixpkgs Rust packaging:
  https://github.com/NixOS/nixpkgs/blob/master/doc/languages-frameworks/rust.section.md
- egui/eframe:
  https://github.com/emilk/egui
- Rust StatusNotifierItem integration:
  https://github.com/iovxw/ksni
