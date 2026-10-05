{ defaultPackage }:
{ config, lib, pkgs, utils, ... }:
let
  cfg = config.programs.moonboot;
  common = import ./common.nix { inherit lib pkgs cfg; defaultPackage = defaultPackage pkgs; };
in {
  options.programs.moonboot = common.options // {
    user = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      example = "amelia";
      description = ''
        Existing normal user whose graphical session may autostart Moonboot.
        Required only for autostart; the user service is gated by ConditionUser.
        Log into the UWSM Hyprland session so graphical-session.target and the
        imported WAYLAND_DISPLAY/session D-Bus environment are available.
      '';
    };
  };
  config = lib.mkIf cfg.enable (lib.mkMerge [
    {
      environment.systemPackages = [ cfg.package ];
      assertions = [
        {
          assertion = !cfg.autostart || (config.programs.hyprland.enable && config.programs.hyprland.withUWSM);
          message = "Moonboot NixOS autostart requires existing programs.hyprland.enable and programs.hyprland.withUWSM; launch the actual UWSM session.";
        }
        {
          assertion = !cfg.autostart || (cfg.user != null && (config.users.users.${cfg.user}.isNormalUser or false));
          message = "Moonboot NixOS autostart requires programs.moonboot.user naming an existing normal user.";
        }
        {
          assertion = !cfg.shortcut || config.programs.hyprland.enable;
          message = "Moonboot NixOS shortcut requires existing programs.hyprland.enable.";
        }
      ];
    }
    (lib.mkIf cfg.shortcut {
      environment.etc."moonboot/${common.fragmentName}".text = common.shortcut;
    })
    (lib.mkIf cfg.autostart {
      systemd.user.services.moonboot = {
        description = "Moonboot idle tray";
        after = [ "graphical-session.target" ];
        partOf = [ "graphical-session.target" ];
        wantedBy = [ "graphical-session.target" ];
        unitConfig = {
          ConditionUser = cfg.user;
          ConditionEnvironment = "WAYLAND_DISPLAY";
        };
        serviceConfig.ExecStart = utils.escapeSystemdExecArgs (common.args "tray");
      };
    })
  ]);
}
