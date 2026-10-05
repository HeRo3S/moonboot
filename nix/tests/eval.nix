# Evaluate real NixOS modules; optionally supply a pinned Home Manager flake
# to run its real module system too, without adding an HM input to Moonboot.
{ flake, nixpkgs, homeManager ? null, system ? "x86_64-linux" }:
let
  pkgs = import nixpkgs { inherit system; };
  lib = pkgs.lib;
  runtimePath = "/run/agenix/moon boot '$HOME% config.toml";
  nixos = settings: (nixpkgs.lib.nixosSystem {
    inherit system;
    modules = [
      flake.nixosModules.default
      {
        system.stateVersion = "26.05";
        boot.loader.grub.enable = false;
        fileSystems."/" = { device = "/dev/test"; fsType = "ext4"; };
        users.users.tester.isNormalUser = true;
      }
      settings
    ];
  }).config;
  valid = config: builtins.all (entry: entry.assertion) config.assertions;
  packageOnly = nixos { programs.moonboot.enable = true; };
  disabled = nixos { programs.moonboot = { autostart = true; shortcut = true; }; };
  active = nixos {
    programs.hyprland = { enable = true; withUWSM = true; };
    programs.moonboot = { enable = true; autostart = true; shortcut = true; user = "tester"; configFile = runtimePath; };
  };
  service = active.systemd.user.services.moonboot;
  noUwsm = nixos {
    programs.hyprland.enable = true;
    programs.moonboot = { enable = true; autostart = true; user = "tester"; };
  };
  noUser = nixos {
    programs.hyprland = { enable = true; withUWSM = true; };
    programs.moonboot = { enable = true; autostart = true; };
  };
  unknownUser = nixos {
    programs.hyprland = { enable = true; withUWSM = true; };
    programs.moonboot = { enable = true; autostart = true; user = "missing"; };
  };
  noHyprland = nixos { programs.moonboot = { enable = true; autostart = true; user = "tester"; }; };
  shortcutOnly = nixos {
    programs.hyprland.enable = true;
    programs.moonboot = { enable = true; shortcut = true; configType = "lua"; };
  };
  hm = osConfig: settings: (homeManager.lib.homeManagerConfiguration {
    inherit pkgs;
    extraSpecialArgs = lib.optionalAttrs (osConfig != null) { inherit osConfig; };
    modules = [
      flake.homeManagerModules.default
      { home = { username = "tester"; homeDirectory = "/home/tester"; stateVersion = "26.05"; }; }
      settings
    ];
  }).config;
  hmManaged = configType: hm null {
    wayland.windowManager.hyprland = { enable = true; inherit configType; };
    programs.moonboot = { enable = true; autostart = true; shortcut = true; configFile = runtimePath; };
  };
  hmSystem = hm { programs.hyprland.enable = true; } ({ config, ... }: {
    programs.moonboot = { enable = true; autostart = true; shortcut = true; configFile = runtimePath; };
    xdg.configFile.hypr.source = config.lib.file.mkOutOfStoreSymlink "/home/tester/dotfiles/hypr";
  });
  hmStandalone = hm null {
    programs.moonboot = { enable = true; integration = "system"; autostart = true; shortcut = true; configType = "lua"; };
  };
  hmPackage = hm null { programs.moonboot.enable = true; };
  hmDisabled = hm null { programs.moonboot = { autostart = true; shortcut = true; }; };
  hmInvalid = hm null { programs.moonboot = { enable = true; autostart = true; }; };
  hmWrongMode = hm null { programs.moonboot = { enable = true; integration = "home-manager"; shortcut = true; }; };
  hmDuplicate = hm { programs = { hyprland.enable = true; moonboot = { enable = true; autostart = true; user = "tester"; }; }; } {
    programs.moonboot = { enable = true; autostart = true; };
  };
  aliceSystem = nixos {
    users.users.alice.isNormalUser = true;
    programs.hyprland = { enable = true; withUWSM = true; };
    programs.moonboot = { enable = true; autostart = true; user = "alice"; configFile = "/run/agenix/alice-moonboot"; };
  };
  bobHome = hm aliceSystem ({ config, ... }: {
    home.username = lib.mkForce "bob";
    home.homeDirectory = lib.mkForce "/home/bob";
    programs.moonboot = { enable = true; autostart = true; shortcut = true; configFile = "/run/agenix/bob-moonboot"; };
    xdg.configFile.hypr.source = config.lib.file.mkOutOfStoreSymlink "/home/bob/dotfiles/hypr";
  });
  hmPreferred = hm { programs.hyprland.enable = true; } {
    wayland.windowManager.hyprland.enable = true;
    programs.moonboot = { enable = true; autostart = true; };
  };
  hmServiceOnly = hm { programs.hyprland = { enable = true; withUWSM = true; }; } {
    programs.moonboot = { enable = true; autostart = true; };
  };
  hmShortcutOnly = hm null {
    programs.moonboot = { enable = true; integration = "system"; shortcut = true; };
  };
in {
  nixos-package-only = valid packageOnly
    && builtins.elem flake.packages.${system}.default packageOnly.environment.systemPackages
    && !(packageOnly.systemd.user.services ? moonboot)
    && !(packageOnly.environment.etc ? "moonboot/hyprland.conf")
    && !packageOnly.programs.hyprland.enable;
  nixos-disabled = valid disabled && !(disabled.systemd.user.services ? moonboot)
    && !(disabled.environment.etc ? "moonboot/hyprland.conf");
  nixos-service = valid active
    && service.after == [ "graphical-session.target" ]
    && service.partOf == [ "graphical-session.target" ]
    && service.wantedBy == [ "graphical-session.target" ]
    && service.unitConfig.ConditionUser == "tester"
    && service.unitConfig.ConditionEnvironment == "WAYLAND_DISPLAY"
    && lib.hasSuffix ''"tray"'' service.serviceConfig.ExecStart
    && lib.hasInfix "$$HOME%%" service.serviceConfig.ExecStart
    && lib.hasInfix "--config" service.serviceConfig.ExecStart
    && lib.hasInfix "ConditionUser=tester" active.systemd.user.units."moonboot.service".text;
  nixos-shortcut = valid shortcutOnly
    && !(shortcutOnly.systemd.user.services ? moonboot)
    && lib.hasInfix "hl.bind" shortcutOnly.environment.etc."moonboot/hyprland.lua".text;
  nixos-assertions = !(valid noUwsm) && !(valid noUser) && !(valid unknownUser) && !(valid noHyprland);
  nixos-runtime-path-type = !(builtins.tryEval (nixos {
    programs.moonboot = { enable = true; configFile = "relative.toml"; };
  }).programs.moonboot.configFile).success
    && !(builtins.tryEval (nixos {
      programs.moonboot = { enable = true; configFile = ./eval.nix; };
    }).programs.moonboot.configFile).success;
} // lib.optionalAttrs (homeManager != null) {
  hm-package-only = valid hmPackage && !(hmPackage.systemd.user.services ? moonboot)
    && !hmPackage.wayland.windowManager.hyprland.enable;
  hm-disabled = valid hmDisabled && !(hmDisabled.systemd.user.services ? moonboot)
    && !(hmDisabled.xdg.configFile ? "moonboot/hyprland.conf");
  hm-managed-hyprlang = valid (hmManaged "hyprlang")
    && lib.hasInfix "exec-once" (hmManaged "hyprlang").wayland.windowManager.hyprland.extraConfig
    && lib.hasInfix "bind = SUPER, M" (hmManaged "hyprlang").wayland.windowManager.hyprland.extraConfig
    && !((hmManaged "hyprlang").systemd.user.services ? moonboot);
  hm-managed-lua = valid (hmManaged "lua")
    && lib.hasInfix ''hl.on("hyprland.start"'' (hmManaged "lua").wayland.windowManager.hyprland.extraConfig
    && lib.hasInfix "hl.bind" (hmManaged "lua").wayland.windowManager.hyprland.extraConfig;
  hm-auto-prefers-hm = valid hmPreferred && !(hmPreferred.systemd.user.services ? moonboot)
    && (lib.hasInfix "exec-once" hmPreferred.wayland.windowManager.hyprland.extraConfig
      || lib.hasInfix ''hl.on("hyprland.start"'' hmPreferred.wayland.windowManager.hyprland.extraConfig);
  hm-system-out-of-store = valid hmSystem
    && !hmSystem.wayland.windowManager.hyprland.enable
    && hmSystem.wayland.windowManager.hyprland.extraConfig == ""
    && hmSystem.xdg.configFile.hypr.enable
    && !(hmSystem.xdg.configFile ? "hypr/hyprland.conf")
    && lib.hasInfix "bind = SUPER, M" hmSystem.xdg.configFile."moonboot/hyprland.conf".text;
  hm-system-service = valid hmSystem
    && hmSystem.systemd.user.services.moonboot.Unit.ConditionEnvironment == "WAYLAND_DISPLAY"
    && hmSystem.systemd.user.services.moonboot.Unit.After == [ "graphical-session.target" ]
    && hmSystem.systemd.user.services.moonboot.Unit.PartOf == [ "graphical-session.target" ]
    && hmSystem.systemd.user.services.moonboot.Install.WantedBy == [ "graphical-session.target" ]
    && lib.hasInfix "$$HOME%%" (lib.concatStringsSep "\n" hmSystem.systemd.user.services.moonboot.Service.ExecStart)
    && lib.hasSuffix ''"tray"'' (lib.concatStringsSep "\n" hmSystem.systemd.user.services.moonboot.Service.ExecStart);
  hm-standalone-system-lua = valid hmStandalone
    && lib.hasInfix "hl.bind" hmStandalone.xdg.configFile."moonboot/hyprland.lua".text
    && hmStandalone.systemd.user.services ? moonboot;
  hm-service-only = valid hmServiceOnly && hmServiceOnly.systemd.user.services ? moonboot
    && !(hmServiceOnly.xdg.configFile ? "moonboot/hyprland.conf");
  hm-shortcut-only = valid hmShortcutOnly && !(hmShortcutOnly.systemd.user.services ? moonboot)
    && hmShortcutOnly.xdg.configFile ? "moonboot/hyprland.conf";
  hm-assertions = !(builtins.tryEval (valid hmInvalid)).success
    && !(builtins.tryEval (valid hmWrongMode)).success
    && !(builtins.tryEval (valid hmDuplicate)).success;
  hm-different-users = valid aliceSystem && valid bobHome
    && aliceSystem.systemd.user.services.moonboot.unitConfig.ConditionUser == "alice"
    && lib.hasInfix "/run/agenix/alice-moonboot" aliceSystem.systemd.user.services.moonboot.serviceConfig.ExecStart
    && lib.hasInfix "/run/agenix/bob-moonboot" (lib.concatStringsSep "\n" bobHome.systemd.user.services.moonboot.Service.ExecStart)
    && bobHome.systemd.user.services.moonboot.Unit.ConditionEnvironment == "WAYLAND_DISPLAY"
    && bobHome.systemd.user.services.moonboot.Unit.PartOf == [ "graphical-session.target" ]
    && bobHome.systemd.user.services.moonboot.Install.WantedBy == [ "graphical-session.target" ]
    && !bobHome.wayland.windowManager.hyprland.enable
    && bobHome.wayland.windowManager.hyprland.extraConfig == ""
    && bobHome.xdg.configFile.hypr.enable
    && !(bobHome.xdg.configFile ? "hypr/hyprland.conf")
    && valid (hm { programs = { hyprland.enable = true; moonboot = { enable = true; autostart = true; }; }; } {
      programs.moonboot = { enable = true; autostart = true; };
    })
    && valid (hm { programs = { hyprland.enable = true; moonboot = { enable = true; autostart = true; user = null; }; }; } {
      programs.moonboot = { enable = true; autostart = true; };
    });
}
