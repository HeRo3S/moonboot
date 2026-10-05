{ defaultPackage }:
args@{ config, lib, pkgs, ... }:
let
  cfg = config.programs.moonboot;
  osConfig = args.osConfig or { };
  hmHyprland = config.wayland.windowManager.hyprland;
  systemHyprland = osConfig.programs.hyprland.enable or false;
  mode = if cfg.integration != "auto" then cfg.integration
    else if hmHyprland.enable then "home-manager"
    else if systemHyprland then "system"
    else "auto";
  common = import ./common.nix { inherit lib pkgs cfg; defaultPackage = defaultPackage pkgs; };
in {
  options.programs.moonboot = common.options // {
    integration = lib.mkOption {
      type = lib.types.enum [ "auto" "home-manager" "system" ];
      default = "auto";
      description = ''
        Auto prefers enabled Home Manager Hyprland, then enabled NixOS Hyprland
        from osConfig. Standalone Home Manager must explicitly select system
        for an externally installed compositor. System mode never manages
        Hyprland or its configuration tree.
      '';
    };
  };
  config = lib.mkIf cfg.enable (lib.mkMerge [
    {
      home.packages = [ cfg.package ];
      assertions = [
        {
          assertion = !(cfg.autostart || cfg.shortcut) || mode != "auto";
          message = "Moonboot integration could not detect Hyprland; enable HM/NixOS Hyprland or select programs.moonboot.integration = \"system\" for standalone HM.";
        }
        {
          assertion = !(cfg.autostart || cfg.shortcut) || mode != "home-manager" || hmHyprland.enable;
          message = "Moonboot home-manager integration requires wayland.windowManager.hyprland.enable.";
        }
        {
          assertion = !cfg.autostart || !((osConfig.programs.moonboot.enable or false)
            && (osConfig.programs.moonboot.autostart or false)
            && (osConfig.programs.moonboot.user or null) == config.home.username);
          message = "Enable Moonboot autostart in only one of the NixOS and Home Manager modules for the same user.";
        }
      ];
    }
    (lib.mkIf (mode == "home-manager" && (cfg.autostart || cfg.shortcut)) {
      wayland.windowManager.hyprland.extraConfig = lib.mkAfter
        (common.managedConfig ((hmHyprland.configType or "hyprlang") == "lua"));
    })
    (lib.mkIf (mode == "system" && cfg.shortcut) {
      xdg.configFile."moonboot/${common.fragmentName}".text = common.shortcut;
    })
    (lib.mkIf (mode == "system" && cfg.autostart) {
      # Requires an actual graphical session and its imported environment.
      # UWSM provides this on NixOS; standalone HM must arrange it itself.
      systemd.user.services.moonboot = {
        Unit = {
          Description = "Moonboot idle tray";
          After = [ "graphical-session.target" ];
          PartOf = [ "graphical-session.target" ];
          ConditionEnvironment = "WAYLAND_DISPLAY";
        };
        Service = {
          ExecStart = lib.hm.strings.escapeSystemdExecArgs (common.args "tray");
        };
        Install.WantedBy = [ "graphical-session.target" ];
      };
    })
  ]);
}
