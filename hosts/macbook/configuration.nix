{ config, pkgs, ... }:

{
  imports = [
    ../common/configuration.nix
  ];

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

  environment = {
    systemPackages = with pkgs; [
      fish
      raycast
    ];

    etc."gitconfig".text = ''
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
  };

  homebrew = {
    enable = true;
    casks = [
      "ghostty"
      "docker"
    ];
    onActivation.cleanup = "zap";
  };

  users = {
    knownUsers = [ "lorenzo" ];
    users.lorenzo.uid = 501;
    users.lorenzo.shell = pkgs.fish;
  };

  nixpkgs.hostPlatform = "aarch64-darwin";
}
