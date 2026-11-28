# Installs `disk-cleanup` on every host so it can also be run manually
# (e.g. `disk-cleanup --dry-run --force`). Scheduling is per-OS in
# hosts/desktop/disk-cleanup.nix (systemd) and hosts/darwin/disk-cleanup.nix
# (launchd).
{
  config,
  pkgs,
  ...
}:
{
  environment.systemPackages = [
    (pkgs.callPackage ../../pkgs/disk-cleanup {
      nix = config.nix.package;
    })
  ];
}
