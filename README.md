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

### One Configuration File

The normal setup is **one file**, with credentials alongside the plug settings:

```toml
[tuya]
endpoint = "https://YOUR_REGIONAL_TUYA_ENDPOINT"
device_id = "YOUR_PLUG_DEVICE_ID"
switch_code = "YOUR_SUPPORTED_BOOLEAN_SWITCH_CODE"
client_id = "YOUR_TUYA_PROJECT_ACCESS_ID"
client_secret = "YOUR_TUYA_PROJECT_ACCESS_SECRET"

[moonlight]
host = "YOUR_HOST_IP_OR_NAME"
```

`app = "Desktop"`, `executable = "moonlight"`, empty stream arguments, enabled
notifications and the startup timings above are defaults. `[startup]` and
`[notifications]` are optional; only add overrides when needed.

The device ID identifies the plug; the client secret authorizes cloud requests.
Knowing the device ID is not equivalent to having those credentials. A separate
secret file is no longer required. Existing `credentials_file` configurations
remain supported, and that optional reference is useful when a nonsecret config
is generated by Nix while agenix supplies only credentials. Never specify both
inline credentials and `credentials_file`, or only half of the inline pair.

A file containing inline credentials, or the optional external credentials file,
must resolve to a user-owned regular file with mode **0600 or 0400**, outside the
Nix store and without hard links. The mode applies to the resolved target, not
the symlink. Unsafe files are rejected without automatically changing permissions.
Use `chmod 600 /private/path/config.toml` for a writable private configuration.

Do not put plaintext credentials in Git, command arguments, derivation
environments, `home.file.text`, or other Nix expressions. Nix store files are
generally readable by other users. A Nix-store config is only supported when it
contains no inline credentials and references a separate private runtime file.

The source whitelist rejects non-source files and symlinks, but **an enclosing
flake can enter the store before filtering**. Keep secrets outside the project
even if `.gitignore` ignores them. Never track credentials.

### Settings In The App

Open `moonboot gui`, then **Settings**. It works even before a configuration
exists. Enter the Tuya and Moonlight fields and choose Save; optional timings,
notifications and the external credentials-file reference are under Advanced.
Client ID and secret are masked, drafts are discarded on Cancel/close/Hide, and
configuration changes are disabled during startup, streaming and off approval.

Saving only validates and writes local configuration. It never authenticates,
probes the host, changes power or launches Moonlight. Successful saves immediately
update the existing controller and clear observed plug status to Unknown. New
files are mode 0600; new configuration directories are mode 0700. Default-valued
optional settings are omitted from the saved TOML to keep it compact. The editor
rewrites TOML, so comments and formatting are not retained. Once a Save begins,
closing or hiding the editor does not roll it back.

Use **Reload from disk** after an external dotfile edit or agenix redeployment.
Stale drafts, externally created files and changed symlink targets are rejected
rather than silently overwritten. Post-commit durability/conflict warnings are
reported honestly; a changed file selection disables operations until Reload.
Unexpected displaced versions are retained as private recovery files. Advisory
locking cannot exclude every non-cooperating external editor.

Read-only `0400`, Nix-store, agenix-managed and encrypted `.age` targets cannot be
saved through Settings. Edit the encrypted/declarative source and redeploy, then
Reload. Moonboot does not encrypt secrets itself or overwrite ciphertext with
plaintext. Quit the existing controller before launching with a different
`--config` filename; repeated GUI invocations otherwise reopen the current one.

### Dotfile Symlinks And Agenix

File symlinks, relative symlink chains and symlinked configuration directories
are supported. Save updates the resolved writable target atomically and preserves
the links. An unencrypted dotfile target must be private and **ignored by Git**;
the app does not modify your repository ignore rules. For Home Manager's editable
checkout pattern:

```nix
{ config, ... }: {
  home.file.".config/moonboot/config.toml".source =
    config.lib.file.mkOutOfStoreSymlink
      "${config.customVars.dotfilesDir}/.config/moonboot/config.toml";
}
```

Do not use a Nix path literal or `builtins.readFile` for plaintext credentials.
If you link the entire Moonboot directory, avoid having Home Manager also claim
files inside it; use a file-only config link when enabling its generated shortcut
fragment, or keep the binding in your own Hyprland dotfiles.
For a safely tracked setup, encrypt the **entire unified TOML file** with agenix
and expose only its decrypted runtime path:

```nix
{ config, inputs, ... }: {
  imports = [ inputs.agenix.nixosModules.default ];
  age.secrets.moonboot = {
    file = ./secrets/moonboot-config.toml.age; # encrypted ciphertext only
    owner = config.customCfg.user.name;
    mode = "0400";
  };

  programs.moonboot.configFile = config.age.secrets.moonboot.path;
}
```

The `customCfg`/`customVars` references above follow the inspected dotfiles
conventions; replace them with your own username/checkout values elsewhere.
Import one of the Moonboot modules below for `programs.moonboot.configFile`.
Alternatively, symlink `~/.config/moonboot/config.toml` to the decrypted path or
use `moonboot gui --config /run/agenix/moonboot`. Agenix's runtime symlink chains
and user-owned `0400` targets are supported. Only ciphertext belongs in Git/the
Nix store; decryption remains agenix's responsibility.

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
the same package in `home.packages`. Moonboot exports both
`nixosModules.default` and `homeManagerModules.default`.

### Hyprland Installed By NixOS

This supports an existing `programs.hyprland` system installation without enabling
Home Manager's Hyprland module or replacing a symlinked `~/.config/hypr` tree:

```nix
{ inputs, config, ... }: {
  imports = [ inputs.moonboot.nixosModules.default ];
  programs.moonboot = {
    enable = true;
    autostart = true; # optional, requires the actual UWSM Hyprland session
    user = config.customCfg.user.name;
    configFile = config.age.secrets.moonboot.path; # optional runtime path
    shortcut = true; # optional example binding; fragment must be sourced
  };
}
```

Package-only installation is simply `programs.moonboot.enable = true`.
Autostart uses a graphical-session-scoped **user** service, gated by username and
`WAYLAND_DISPLAY`. It requires existing `programs.hyprland.enable` and
`programs.hyprland.withUWSM`; log into the UWSM session so its graphical target
and imported Wayland/session D-Bus environment actually exist. The module never
enables a compositor or installs a system boot daemon.

With the optional hyprlang shortcut, source the generated fragment from your own
linked compositor configuration:

```ini
source = /etc/moonboot/hyprland.conf
```

Set `programs.moonboot.configType = "lua"` for a Lua fragment instead, and load
`/etc/moonboot/hyprland.lua` using your Lua configuration's loading mechanism.
No source line is inserted automatically into your dotfiles.

### Home Manager Integration

Home Manager can manage Moonboot with either compositor ownership arrangement:

```nix
home-manager.extraSpecialArgs = { inherit inputs; };
home-manager.users.your-user = {
  imports = [ inputs.moonboot.homeManagerModules.default ];
  programs.moonboot = {
    enable = true;
    integration = "auto"; # prefers HM-managed Hyprland, then NixOS Hyprland
    autostart = true;
    shortcut = true; # optional SUPER+M example
  };
};
```

When Home Manager manages Hyprland, this appends the idle `exec-once`/Lua startup
hook and GUI binding, respecting its configuration format. When NixOS manages
Hyprland, `auto` detects it through `osConfig` and uses a graphical-session user
service without taking ownership of the linked config directory. Standalone Home
Manager with an externally installed compositor can select `integration = "system"`
and must arrange the graphical target/imported environment itself.

System mode writes an optional shortcut fragment under
`~/.config/moonboot/hyprland.conf` (or `.lua` with `configType = "lua"`); source it
from your own compositor config. `configFile` can reference the agenix runtime
path through `osConfig.age.secrets.moonboot.path` in a NixOS-integrated HM module.

Both autostart and shortcut default off. Enable autostart in **only one** Moonboot
module/launch path for each user; conflicting same-user NixOS/HM declarations are
rejected. No module alters Waybar, installs secret contents, or adds a logout,
shutdown or stream-exit power action.

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
