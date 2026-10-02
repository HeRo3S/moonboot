{
  description = "Safety-first Tuya-to-Moonlight launcher";
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      system = "x86_64-linux";
      pkgs = import nixpkgs { inherit system; };
      lib = pkgs.lib;
      allowedFiles = [
        "Cargo.toml" "Cargo.lock"
        "src/lib.rs" "src/main.rs" "src/backend.rs" "src/backend_tests.rs"
        "src/config.rs" "src/cloud.rs" "src/process.rs" "src/ui.rs"
        "src/controller.rs" "src/demo.rs"
        "tests/ui.rs" "tests/cli.rs" "tests/controller.rs"
      ];
      sourceFilter = path: type:
        let relative = lib.removePrefix "${toString ./.}/" (toString path);
        in if type == "directory" then
          builtins.any (file: lib.hasPrefix "${relative}/" file) allowedFiles
        else type == "regular" && builtins.elem relative allowedFiles;
      src = builtins.path { path = ./.; name = "moonboot-source"; filter = sourceFilter; };
      runtimeLibraries = [ pkgs.libGL pkgs.libxkbcommon pkgs.wayland ];
      testBusConfig = pkgs.writeText "moonboot-test-bus.conf" ''
        <busconfig><type>session</type><listen>unix:tmpdir=/tmp</listen>
        <auth>EXTERNAL</auth><policy context="default">
        <allow send_destination="*"/><allow receive_sender="*"/><allow own="*"/>
        </policy></busconfig>
      '';
      isolatedTests = ''
        export HOME="$TMPDIR/test-home"
        export XDG_CONFIG_HOME="$HOME/config"
        export XDG_STATE_HOME="$HOME/state"
        export XDG_RUNTIME_DIR="$TMPDIR/test-runtime"
        mkdir -p "$XDG_CONFIG_HOME" "$XDG_STATE_HOME" "$XDG_RUNTIME_DIR"
        chmod 700 "$XDG_RUNTIME_DIR"
        unset DISPLAY WAYLAND_DISPLAY DBUS_SESSION_BUS_ADDRESS
      '';
      moonboot = pkgs.rustPlatform.buildRustPackage {
        pname = "moonboot";
        version = "0.1.0";
        inherit src;
        cargoLock.lockFile = ./Cargo.lock;
        nativeBuildInputs = [ pkgs.makeWrapper pkgs.pkg-config ];
        nativeCheckInputs = [ pkgs.dbus ];
        buildInputs = [ pkgs.wayland pkgs.libxkbcommon ];
        preCheck = isolatedTests;
        postFixup = ''
          wrapProgram "$out/bin/moonboot" \
            --prefix PATH : ${lib.makeBinPath [ pkgs.moonlight-qt pkgs.libnotify ]} \
            --prefix LD_LIBRARY_PATH : ${lib.makeLibraryPath runtimeLibraries}
        '';
        meta = {
          description = "Safe Tuya power-on and Moonlight session launcher";
          homepage = "https://github.com/HeRo3S/moonboot";
          license = lib.licenses.mit;
          mainProgram = "moonboot";
          platforms = [ system ];
        };
      };
      quality = moonboot.overrideAttrs (old: {
        pname = "moonboot-quality";
        nativeBuildInputs = old.nativeBuildInputs ++ [ pkgs.rustfmt pkgs.clippy ];
        buildPhase = ''
          runHook preBuild
          cargo fmt --all -- --check
          cargo clippy --all-targets --frozen --offline -- -D warnings
          runHook postBuild
        '';
        checkPhase = ''
          runHook preCheck
          cargo test --frozen --offline
          runHook postCheck
        '';
        installPhase = ''mkdir -p "$out"; touch "$out/passed"'';
        postFixup = "";
      });
    in {
      packages.${system} = { default = moonboot; inherit moonboot; };
      apps.${system}.default = { type = "app"; program = lib.getExe moonboot; meta.description = moonboot.meta.description; };
      devShells.${system}.default = pkgs.mkShell {
        packages = [ pkgs.cargo pkgs.rustc pkgs.rustfmt pkgs.clippy pkgs.pkg-config
          pkgs.moonlight-qt pkgs.libnotify pkgs.dbus ];
        buildInputs = [ pkgs.wayland pkgs.libxkbcommon ];
        LD_LIBRARY_PATH = lib.makeLibraryPath runtimeLibraries;
      };
      checks.${system} = {
        inherit quality;
        package = moonboot;
        source-safety = assert builtins.all (relative: !(sourceFilter "${toString ./.}/${relative}" "regular")) [
          "credentials.toml" "src/secret.rs" "src/credentials.toml" ".env" "target/debug/moonboot" "config.toml"
        ]; assert !(sourceFilter "${toString ./.}/src/main.rs" "symlink");
        pkgs.runCommand "moonboot-source-safety" { } ''
          test ! -e ${src}/credentials.toml
          test ! -e ${src}/src/secret.rs
          test ! -e ${src}/config.toml
          test ! -e ${src}/target
          mkdir -p "$out"
        '';
        package-smoke = pkgs.runCommand "moonboot-package-smoke" { nativeBuildInputs = [ pkgs.dbus ]; } ''
          export HOME="$TMPDIR/home"
          export XDG_RUNTIME_DIR="$TMPDIR/runtime"
          export XDG_STATE_HOME="$HOME/state"
          mkdir -p "$HOME" "$XDG_RUNTIME_DIR"
          chmod 700 "$XDG_RUNTIME_DIR"
          unset DISPLAY WAYLAND_DISPLAY DBUS_SESSION_BUS_ADDRESS
          ${lib.getExe moonboot} --help
          ${lib.getExe moonboot} --version
          # The package must start its inert demo tray outside the development shell.
          dbus-run-session --config-file ${testBusConfig} -- sh -c '
            PATH=/nonexistent ${lib.getExe moonboot} tray --demo success &
            child=$!
            trap "kill -TERM $child 2>/dev/null || true" EXIT
            sleep 1
            kill -0 "$child"
            kill -TERM "$child"
            wait "$child"
          '
          mkdir -p "$out"
        '';
      };
      homeManagerModules.default = { config, lib, pkgs, ... }:
        let
          cfg = config.programs.moonboot;
          exe = lib.getExe cfg.package;
          lua = config.wayland.windowManager.hyprland.configType == "lua";
        in {
          options.programs.moonboot = {
            enable = lib.mkEnableOption "Moonboot";
            package = lib.mkOption {
              type = lib.types.package;
              default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
            };
            autostart = lib.mkEnableOption "idle tray startup with Hyprland";
            shortcut = lib.mkEnableOption "SUPER M controls shortcut (example binding)";
          };
          config = lib.mkIf cfg.enable {
            home.packages = [ cfg.package ];
            assertions = [{
              assertion = !(cfg.autostart || cfg.shortcut) || config.wayland.windowManager.hyprland.enable;
              message = "Moonboot autostart/shortcut requires Home Manager Hyprland to be enabled";
            }];
            wayland.windowManager.hyprland.extraConfig = lib.mkIf (cfg.autostart || cfg.shortcut) (lib.mkAfter (
              if lua then
                lib.optionalString cfg.autostart ''
                  hl.on("hyprland.start", function() hl.exec_cmd("${exe} tray") end)
                '' + lib.optionalString cfg.shortcut ''
                  hl.bind("SUPER + M", hl.dsp.exec_cmd("${exe} gui"))
                ''
              else
                lib.optionalString cfg.autostart ''
                  exec-once = ${exe} tray
                '' + lib.optionalString cfg.shortcut ''
                  bind = SUPER, M, exec, ${exe} gui
                ''
            ));
          };
        };
    };
}
