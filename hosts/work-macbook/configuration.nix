# The work Mac. Everything both Macs share is in ../darwin. No linux-builder,
# attic or sops here, and none of the personal casks.
{ lib, pkgs, ... }:

{
  imports = [
    ../darwin/configuration.nix
  ];

  environment.systemPackages = [
    pkgs.awscli2
    pkgs.slack
  ];

  homebrew.casks = [ "granola" ];

  # Oneleet's agent checks the firewall (below) and the screen lock. Oneleet
  # itself is installed from its own .pkg: it is in neither nixpkgs nor
  # Homebrew, and it runs root daemons and updates itself.
  #
  # Oneleet wants the Mac locked within 10 minutes of going idle: display
  # sleep plus the password delay (10 s, from ../darwin) must stay under that.
  power.sleep.display = lib.mkForce 5;

  # darwin-rebuild picks the flake config by LocalHostName, so this has to
  # match darwinConfigurations.work-macbook for a bare `darwin-rebuild switch`.
  networking = {
    hostName = "work-macbook";
    localHostName = "work-macbook";
    computerName = "work-macbook";
    applicationFirewall.enable = true;
  };
}
