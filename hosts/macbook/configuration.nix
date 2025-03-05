{ config, pkgs, ... }:

{

  imports = [
    ../common/configuration.nix
  ];

  nix.settings.experimental-features = "nix-command flakes";
  nixpkgs.config.allowUnfree = true;

  system = {
    keyboard = {
      enableKeyMapping = true;
      remapCapsLockToEscape = true;
    };
    defaults = {
      dock.autohide = true;
      finder.FXPreferredViewStyle = "clmv";
      screensaver.askForPasswordDelay = 10;
      NSGlobalDomain.AppleICUForce24HourTime = true;
      NSGlobalDomain.AppleInterfaceStyle = "Dark";
      NSGlobalDomain.KeyRepeat = 2;
    };
    stateVersion = 6;
  };

  environment.systemPackages = with pkgs; [
    raycast
  ];

  homebrew = {
    enable = true;
    casks = [
      "ghostty"
    ];
    onActivation.cleanup = "zap";
  };

  fonts.packages = [
    pkgs.nerd-fonts.fira-code
  ];

  programs.fish.enable = true;
  users.knownUsers = [ "lorenzo" ];
  users.users.lorenzo.uid = 501;
  users.users.lorenzo.shell = pkgs.fish;

  environment.variables = {
    EDITOR = "nvim";
    PKG_CONFIG_PATH = "${pkgs.postgresql}/lib/pkgconfig";
    LIBRARY_PATH = "${pkgs.postgresql.lib}/lib";
  };

  environment.etc."gitconfig".text = ''
    [user]
    name = Lorenzo
    email = maturanolorenzo@gmail.com
    signingKey = /Users/lorenzo/.ssh/id_rsa.pub

    [gpg]
    format = ssh

    [gpg "ssh"]
    allowedSignersFile = /Users/lorenzo/.ssh/allowed-signers

    [commit]
    gpgSign = true
  '';

  services.openssh.enable = true;
  services.tailscale.enable = true;

  nixpkgs.hostPlatform = "aarch64-darwin";
}
