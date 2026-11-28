# Hourly threshold-triggered cleanup for safe Nix and Rust artifacts on macOS
# (shared by macbook and work-macbook).
#
# Runs as a root launchd daemon so it can prune old nix-darwin system
# generations under /nix/var/nix/profiles/system as well as run
# nix-collect-garbage; everything under /Users/lorenzo and macOS notifications
# run as `lorenzo`. Quiet when disk usage is below 85%; logs to
# /var/log/disk-cleanup.log and notifies via terminal-notifier when space is
# freed.
{
  config,
  lib,
  pkgs,
  ...
}:
let
  user = config.system.primaryUser;
  package = pkgs.callPackage ../../pkgs/disk-cleanup {
    nix = config.nix.package;
  };
in
{
  launchd.daemons.disk-cleanup.serviceConfig = {
    ProgramArguments = [
      "${lib.getExe package}"
      "--user"
      user
    ];
    # Hourly at :17; launchd coalesces missed runs when the Mac wakes from sleep.
    StartCalendarInterval = [ { Minute = 17; } ];
    LowPriorityIO = true;
    LowPriorityBackgroundIO = true;
    Nice = 10;
    ProcessType = "Background";
    StandardOutPath = "/var/log/disk-cleanup.log";
    StandardErrorPath = "/var/log/disk-cleanup.log";
  };

  # Bound /var/log/disk-cleanup.log via macOS newsyslog (rotate at 1 MiB,
  # keep 3 gzipped archives, N = no daemon to signal for a oneshot job).
  environment.etc."newsyslog.d/disk-cleanup.conf".text = ''
    # logfilename              [owner:group]  mode count size  when  flags
    /var/log/disk-cleanup.log  root:wheel     644  3     1024  *     ZN
  '';
}
