{ config, pkgs, ... }:

{
  imports = [
    ../common/configuration.nix
  ];

  nixpkgs.overlays = [
    (final: prev: {
      spotify = prev.spotify.overrideAttrs (oldAttrs: {
        src = prev.fetchurl {
          url = "https://download.scdn.co/SpotifyARM64.dmg";
          sha256 = "sha256-cslyAkpAXsVvIfx7tsDpDxnSjidH2uHCeFBq3pXFaMo=";
        };
      });
    })
  ];

  system = {
    primaryUser = "lorenzo";
    keyboard = {
      enableKeyMapping = true;
      remapCapsLockToEscape = true;
    };
    defaults = {
      dock = {
        persistent-apps = [
          "/Applications/Ghostty.app"
          "/Applications/Slack.app"
        ];
        autohide = true;
        show-recents = false;
        show-process-indicators = false;
      };
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
      "slack"
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
