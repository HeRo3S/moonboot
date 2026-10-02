# Moonboot

A small Rust tray controller for starting a Moonlight session through a Tuya
smart plug. Built for **NixOS, Hyprland and Waybar**, with a terminal CLI and an
egui Wayland controls window. Native Moonlight Qt is an external client.

## Safety First

- Start Session sends only explicit **on**, and only if the plug reports off.
- An already-on plug is never toggled or power-cycled.
- Cancel, errors, timeouts, stream exit, Hide and Quit never switch the plug off.
- Plug Off is **not Linux shutdown**. Shut the host down normally first, verify
  completion independently, then type exactly `POWER OFF` and confirm.
- Piped input, `--yes`, remembered approval and tray one-click off are not supported.
- Cloud acceptance and plug-on do not prove that Linux or Sunshine is running.
- Login starts only an idle tray: no config/secret reads, cloud authentication,
  polling, power command, notification or Moonlight launch in the normal case.
- The operation lock cannot prevent power changes by other apps or automation.
  Human approval is not automatic shutdown verification.

## Try Without Hardware

```sh
nix run . -- gui --demo success
nix run . -- gui --demo already-on
nix run . -- gui --demo cloud-error
nix run . -- gui --demo readiness-timeout
nix run . -- gui --demo off-error
nix run . -- tray --demo success
```

Demo selects fake dependencies **before** reading any production configuration.
It never reads real credentials, contacts Tuya/the host, or launches Moonlight.
Each scenario has separate UI IPC, operation locks and private state/log identity,
distinct from production. The panel, tray menu and failure notifications say DEMO.

| Scenario | Repeatable behavior |
| --- | --- |
| `success` | Starts off; simulated power-on, boot wait, then simulated stream |
| `already-on` | Starts on; no simulated on command, then simulated stream |
| `cloud-error` | Operations fail without contacting any service |
| `readiness-timeout` | Simulated power-on followed by a readiness failure |
| `off-error` | Starts on; approved off reports uncertainty without retries |

Use **End Demo Session** to end a simulated stream and reach Plug Off. Its dialog
still requires fresh typed approval. Restart the controls process to reset the
scenario; reopening a running scenario intentionally preserves its state. A demo
can never become live. Launch production separately and explicitly.

## Prerequisites

1. Host BIOS is configured to power on when AC is restored.
2. Host Linux autologin and Sunshine startup are already configured.
3. Host and client have usable connectivity, directly or through an existing VPN.
4. Moonlight is already paired under the same desktop user/profile as Moonboot.
5. Tuya developer project, app-account linking and relevant API entitlement are ready.
6. Waybar is running with `"tray"` in its modules. Hyprland itself has no tray.
7. Run as the logged-in user with the existing Wayland, session D-Bus and
   `XDG_RUNTIME_DIR` environment, not as root or a system boot service.

The packaged target uses stable Rust 1.98 and a modern Linux kernel (5.9+ with
`pidfd_open`/`close_range` available). Process-group guardians clean up supervised
clients even if their controller is forcibly killed. Unsupported or denied guardian
setup fails before a stream or power-on workflow can proceed; processes deliberately
detaching into another session/group are outside that cleanup boundary.

Moonboot does not provision Tuya, pair Moonlight, configure BIOS, shut down Linux,
enable a VPN, alter a firewall or expose Sunshine to the internet.

## Configuration

Use [`config.example.toml`](config.example.toml) as the configuration reference.
The default path is `$XDG_CONFIG_HOME/moonboot/config.toml`, falling back to
`~/.config/moonboot/config.toml`. `--config /absolute/path/config.toml` is a global
option accepted before or after the command. `~/` expansion is explicit.

Fill in the **regional HTTPS origin**, device ID, exact supported boolean switch
code, host address and exact Sunshine app name. No switch code or host is guessed.
Default readiness timeout is 180 seconds; no additional stream flags are supplied.
Arguments containing spaces are passed as individual arguments, never shell code.

The runtime credentials file is a separate user-owned, mode-0600 regular file,
outside the repository and Nix store:

```toml
client_id = "YOUR_TUYA_PROJECT_ACCESS_ID"
client_secret = "YOUR_TUYA_PROJECT_ACCESS_SECRET"
```

Set its permissions explicitly with `chmod 600 /private/path/tuya-credentials.toml`.
Unsafe ownership/permissions, symlinks and hard links are rejected; Moonboot never
silently changes them. Do not enter secrets in command arguments, Git, derivation
environments, `home.file` contents or other Nix expressions. Nix store files are
generally readable by other users. Optional sops-nix/agenix setup must supply a
private regular runtime file meeting these checks; it is not required.

The source whitelist rejects non-source files and symlinks, but **an enclosing
flake can enter the store before filtering**. Keep secrets outside the project
even if `.gitignore` ignores them. Never track credentials.

## Commands

```sh
nix run . -- check
nix run . -- start
nix run . -- plug-off       # interactive terminal only, after Linux shutdown
nix run . -- tray          # silent idle controller
nix run . -- gui           # open/reopen controls, does not start a session
nix run . -- --help
nix run . -- --version
```

`check` validates config, executable, credentials, cloud access, device online
state, supported boolean switch and reported power. An offline PC is not an
error. It neither changes power nor probes/launches a host stream.

`start` validates dependencies **before** power changes, takes the shared lock,
authenticates, checks control/status, requests on if needed and confirms reported
on with bounded reads. It then immediately runs `moonlight list HOST` until a
successful response contains the exact app name, and launches
`moonlight stream HOST APP` with the configured extra arguments. It supervises
Moonlight until exit and holds the operation lock throughout the session.

`plug-off` does not require Moonlight installed. It validates the plug under the
same lock, does nothing if already off, then asks for fresh interactive approval
before sending false. Ambiguous commands are followed by status reads only;
power commands are never blindly retried, toggled or reversed.

Ctrl+C/SIGTERM cancels HTTP, waits and child processes, and reaps the supervised
client. Cancellation cannot undo a request already delivered to Tuya and can
leave the host booting/running. It is never permission to cut power immediately.

| Exit | Meaning |
| --- | --- |
| 0 | Success |
| 2 | Configuration, dependency or argument failure |
| 3 | Cloud failure or unconfirmed power state |
| 4 | Sunshine readiness timeout |
| 5 | Operation/UI instance busy |
| 6 | Moonlight probe/app/stream failure |
| 7 | Power-off approval declined |
| 130 | Cancelled |

`--verbose` adds safe diagnostics. Credentials, tokens, signed headers, cloud
response bodies and subprocess stderr are not exposed in diagnostics.

## Desktop Controls

The tray menu offers **Open Controls** and **Quit**. No tray menu action sends a
power command. The compact normal Wayland window offers Start Session, Cancel
Startup, Refresh Status, Plug Off and Hide. Network/process work stays off the UI
thread. Idle status is Unknown until requested; observations include timestamp
and age and are not presented as live PC-running detection.

Close/Hide destroys only the native window and keeps the controller and local
startup or streaming running. This avoids Wayland's unsupported window visibility
toggle. Reopen via the tray menu or
`moonboot gui`; duplicate requests reuse the existing panel. If using the trayless
`gui` fallback, rerun the same GUI command to recover a hidden panel. Hiding an
off confirmation declines it, including when validation completes after hiding.
Controls Quit exits controls; tray Quit also cancels controls in the matching
production/demo namespace. Neither shuts down the host or cuts AC power.

The tray and panel have separate per-user locks; idle UI never holds the operation
lock. Private mode-0600 IPC checks the peer UID and accepts only Open/Quit, never
Start or off authorization. A missing tray host logs an actionable diagnostic
and waits for recovery; use `moonboot gui` rather than expecting a login window.

Best-effort `notify-send` notifications do not gate session success. Failures are
also recorded in bounded private `backend.log`/`frontend.log` under
`$XDG_STATE_HOME/moonboot` (fallback `~/.local/state/moonboot`). Demo frontend logs
are in `demo-SCENARIO-moonboot` directories. Missing notification services do not
prevent streaming. No notification is sent per readiness poll.

## Declarative Installation

Only `x86_64-linux` is currently packaged and checked. Add Moonboot to your existing
system flake, passing the input to modules via `specialArgs`:

```nix
{
  inputs.moonboot.url = "github:HeRo3S/moonboot";

  outputs = inputs@{ nixpkgs, ... }: {
    nixosConfigurations.my-client = nixpkgs.lib.nixosSystem {
      system = "x86_64-linux";
      specialArgs = { inherit inputs; };
      modules = [ ./configuration.nix ];
    };
  };
}
```

In the system module:

```nix
{ inputs, pkgs, ... }: {
  environment.systemPackages = [
    inputs.moonboot.packages.${pkgs.stdenv.hostPlatform.system}.default
  ];
}
```

Alternatively pass `inputs` through Home Manager's `extraSpecialArgs` and install
the same package in `home.packages`. The optional integration module is opt-in:

```nix
home-manager.extraSpecialArgs = { inherit inputs; };
home-manager.users.your-user = {
  imports = [ inputs.moonboot.homeManagerModules.default ];
  programs.moonboot = {
    enable = true;
    autostart = true;  # optional: idle tray only
    shortcut = true;  # optional example: SUPER+M, choose for your own bindings
  };
};
```

This respects Home Manager's Hyprland Lua/hyprlang format selection. It does not
enable Hyprland, alter Waybar, install secrets or add system services. Both
autostart and shortcut default off. On older Home Manager without `configType`,
use the manual snippet below rather than this module. Do not enable two autostart
paths. No logout, shutdown or stream-exit hook ever calls plug-off.

For manual hyprlang configuration, substitute the installed launcher path:

```ini
exec-once = /installed/path/bin/moonboot tray
bind = SUPER, M, exec, /installed/path/bin/moonboot gui
# Optional on Hyprland versions using this windowrule syntax:
windowrule = float, class:^(moonboot)$
windowrule = size 520 440, class:^(moonboot)$
```

Hyprland may tile or place/focus the window according to compositor policy;
Moonboot does not force absolute placement. Adjust rules for your compositor
version. No desktop configuration is installed or edited automatically.

The Nix wrapper supplies native Moonlight's verified `moonlight` executable,
libnotify and the glow/Wayland graphics libraries without relying on an interactive
shell PATH. It preserves the user's working desktop environment. Qt/Python are
not needed by Moonboot; Moonlight itself is Qt-based.

## Development And Tests

```sh
nix develop
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
nix build
nix flake check
```

Stable Rust only. Rust/Cargo and Nixpkgs are pinned by the flake; crate resolution
is pinned by `Cargo.lock`. The checks cover fmt/Clippy, fake cloud workflows,
signing/response parsing, deterministic deadlines, real fake-process termination,
locks, private files, demo isolation, PTY confirmation, isolated D-Bus tray startup
and AccessKit-based egui interactions. They never use production credentials or
control a real plug. Initial dependency fetching is distinct from offline tests.

The UI harness does **not** prove Wayland rendering or tray lifecycle. Desktop
demo checks and controlled hardware checks are separate layers. See
[`VERIFICATION.md`](VERIFICATION.md) for recorded results and pending validation,
and [`plan/TESTING_PLAN.md`](plan/TESTING_PLAN.md) for the acceptance checklist.

## Limitations And Troubleshooting

- An on plug cannot produce a new AC-restoration edge for an off PC. A readiness
  timeout explains this possibility without assuming the cause or cutting power.
- App parsing matches exact UTF-8 lines from Moonlight Qt's `listapps.cpp`.
  Pairing and connection failures share exit codes upstream; nonzero probes retry
  to the deadline rather than parsing localized error text. A successful list
  without the app fails immediately. Verify with `moonlight list HOST` directly.
- Initial pairing must be done in the user's real Moonlight profile. Demo cannot
  prove pairing, BIOS boot, device entitlement or real streaming.
- Tuya project product authorization varies. The implementation uses token,
  device details (online flag), functions, status and commands under `/v1.0`.
  Check your project's API permissions/entitlement if access is denied.
- Authentication refresh retries safe reads once. Commands are not retried on
  token errors or ambiguous replies. Check reported state before another operation.
- Configure a correct regional HTTPS origin and synchronized system clock. TLS
  verification stays enabled and redirects cannot forward authorization headers.
- Live tests require explicit approval and private setup values. An off test also
  requires independently completed Linux shutdown and normal typed confirmation.

## Protocol References

- [Tuya cloud signing](https://developer.tuya.com/en/docs/iot/singnature?id=Kbw0q34cs2e5g)
- [Moonlight Qt CLI](https://github.com/moonlight-stream/moonlight-qt/tree/master/app/cli)
- [egui/eframe](https://github.com/emilk/egui)
- [ksni StatusNotifierItem](https://github.com/iovxw/ksni)

MIT licensed.
