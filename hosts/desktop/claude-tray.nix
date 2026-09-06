{
  lib,
  pkgs,
  claude-tray,
  ...
}:

let
  package = claude-tray.packages.${pkgs.stdenv.hostPlatform.system}.default;
in
{
  environment.systemPackages = [ package ];

  systemd.user.services.claude-tray = {
    description = "Claude Code session tray applet";
    documentation = [ "https://github.com/lorenzolfm/claude-tray" ];

    requires = [ "dbus.socket" ];
    after = [ "dbus.socket" ];

    wantedBy = [ "default.target" ];

    environment.PATH = lib.mkForce (
      lib.concatStringsSep ":" [
        "/etc/profiles/per-user/lorenzo/bin"
        "/run/current-system/sw/bin"
      ]
    );

    serviceConfig = {
      ExecStart = lib.getExe package;
      Restart = "always";
      RestartSec = 5;
    };

    unitConfig = {
      ConditionUser = "lorenzo";
      StartLimitIntervalSec = 0;
    };
  };
}
