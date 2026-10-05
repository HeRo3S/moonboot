{ lib, pkgs, cfg, defaultPackage }:
let
  args = command: [ (lib.getExe cfg.package) ]
    ++ lib.optionals (cfg.configFile != null) [ "--config" cfg.configFile ]
    ++ [ command ];
  # Keep runtime paths out of Hyprland's parser; only shell-quoted arguments
  # appear in these launchers, and no runtime configuration is read by Nix.
  launcher = command: pkgs.writeShellScript "moonboot-${command}" ''
    exec ${lib.escapeShellArgs (args command)}
  '';
  gui = launcher "gui";
  tray = launcher "tray";
in {
  inherit args;
  options = {
    enable = lib.mkEnableOption "Moonboot";
    package = lib.mkOption {
      type = lib.types.package;
      default = defaultPackage;
      description = "Moonboot package to install and launch.";
    };
    configFile = lib.mkOption {
      type = lib.types.nullOr (lib.types.addCheck lib.types.str
        (value: lib.hasPrefix "/" value && !(lib.hasInfix "\n" value) && !(lib.hasInfix "\r" value)));
      default = null;
      example = "/run/agenix/moonboot";
      description = ''
        Absolute runtime configuration filename, as a string (not a Nix path).
        Passed via --config without reading or copying its contents into the store.
        May reference age.secrets.moonboot.path; the logged-in user must be able
        to read it. Null uses Moonboot's normal XDG configuration discovery.
        Credentials are never configured through module options.
      '';
    };
    autostart = lib.mkEnableOption "the idle tray in the graphical user session" // {
      description = ''
        Start only the idle tray, never a power or streaming operation.
        System integration uses graphical-session.target and requires an actual
        graphical session with WAYLAND_DISPLAY and the session D-Bus environment
        imported into the user manager. NixOS UWSM supplies this when you log into
        its Hyprland session; enabling UWSM alone does not create that session.
        Standalone Home Manager must arrange the target/environment itself.
        Enable autostart in only one Moonboot module.
      '';
    };
    shortcut = lib.mkEnableOption "the example SUPER M GUI shortcut" // {
      description = ''
        Opt into an example SUPER M binding that opens GUI controls only.
        Home Manager integration appends it to the managed Hyprland config.
        System integration writes a separate moonboot/hyprland.conf (or .lua)
        fragment under the XDG config directory for HM, or /etc for NixOS.
        Source that fragment from your own config; this module never modifies
        an externally managed or out-of-store-symlinked Hyprland config tree.
      '';
    };
    configType = lib.mkOption {
      type = lib.types.enum [ "hyprlang" "lua" ];
      default = "hyprlang";
      description = "Format of the separate system-mode shortcut fragment. Source it from your own Hyprland configuration.";
    };
  };
  fragmentName = "hyprland.${if cfg.configType == "lua" then "lua" else "conf"}";
  shortcut = if cfg.configType == "lua" then ''
    hl.bind("SUPER + M", hl.dsp.exec_cmd("${gui}"))
  '' else ''
    bind = SUPER, M, exec, ${gui}
  '';
  managedConfig = lua:
    lib.optionalString cfg.autostart (if lua then ''
      hl.on("hyprland.start", function() hl.exec_cmd("${tray}") end)
    '' else ''
      exec-once = ${tray}
    '')
    + lib.optionalString cfg.shortcut (if lua then ''
      hl.bind("SUPER + M", hl.dsp.exec_cmd("${gui}"))
    '' else ''
      bind = SUPER, M, exec, ${gui}
    '');
}
