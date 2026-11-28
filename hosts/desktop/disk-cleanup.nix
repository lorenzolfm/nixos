# Hourly threshold-triggered cleanup for safe Nix and Rust artifacts.
#
# Runs as root so it can prune old NixOS system generations and stale
# /tmp/nix-* build dirs; everything under /home/lorenzo and desktop
# notifications drop privileges to `lorenzo`. Quiet when disk usage is below
# 85%; logs to journald and notifies via libnotify when space is freed.
{
  config,
  lib,
  pkgs,
  ...
}:
let
  user = "lorenzo";
  home = "/home/${user}";
  package = pkgs.callPackage ../../pkgs/disk-cleanup {
    nix = config.nix.package;
  };
in
{
  systemd.services.disk-cleanup = {
    description = "Clean safe Nix and Rust artifacts when disk usage is high";

    # The timer owns when this runs. Without this, a rebuild that changes the
    # unit would block `nixos-rebuild` on a full GC/cleanup pass.
    restartIfChanged = false;

    serviceConfig = {
      Type = "oneshot";
      ExecStart = "${lib.getExe package} --user ${user}";
      RuntimeDirectory = "disk-cleanup";
      TimeoutStartSec = "1h";

      Nice = 10;
      IOSchedulingClass = "idle";

      # Keep /home read-only except the exact directories disk-cleanup touches.
      # Do not enable PrivateTmp: cleaning stale /tmp/nix-* requires the host /tmp.
      ProtectHome = "read-only";
      ReadWritePaths = [
        "-${home}/Projects"
        "-${home}/.local/state/nix/profiles"
        "-${home}/.cache/nix"
      ];
      ProtectKernelTunables = true;
      ProtectKernelModules = true;
      ProtectControlGroups = true;
      RestrictNamespaces = true;
      LockPersonality = true;
      SystemCallArchitectures = "native";
    };
  };

  systemd.timers.disk-cleanup = {
    description = "Check disk usage hourly and clean safe Nix and Rust artifacts";
    wantedBy = [ "timers.target" ];
    timerConfig = {
      # Off-peak minute: :00/:30 are crowded and oss-board runs at :07.
      OnCalendar = "*-*-* *:17:00";
      Persistent = true;
      RandomizedDelaySec = "3m";
      Unit = "disk-cleanup.service";
    };
  };
}
