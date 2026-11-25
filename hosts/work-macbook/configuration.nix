# The work Mac. Everything both Macs share is in ../darwin. No linux-builder,
# attic or sops here, and none of the personal casks.
{ pkgs, ... }:

{
  imports = [
    ../darwin/configuration.nix
  ];

  environment.systemPackages = [ pkgs.slack ];

  # darwin-rebuild picks the flake config by LocalHostName, so this has to
  # match darwinConfigurations.work-macbook for a bare `darwin-rebuild switch`.
  networking = {
    hostName = "work-macbook";
    localHostName = "work-macbook";
    computerName = "work-macbook";
  };
}
