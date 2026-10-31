{ config, pkgs, ... }:

{
  imports = [
    ../common/configuration.nix
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

  # darwin-rebuild picks the flake config by LocalHostName, so this has to
  # match darwinConfigurations.macbook for a bare `--flake ~/.config/nixos`.
  networking = {
    hostName = "macbook";
    localHostName = "macbook";
    computerName = "macbook";
  };

  environment = {
    systemPackages = with pkgs; [
      raycast
    ];

    etc."gitconfig".text = ''
      [user]
      name = Lorenzo
      email = maturanolorenzo@gmail.com
      signingKey = /Users/lorenzo/.ssh/id_ed25519.pub

      [gpg]
      format = ssh

      [gpg "ssh"]
      allowedSignersFile = /Users/lorenzo/.ssh/allowed-signers

      [commit]
      gpgSign = true
    '';
  };

  programs.ssh.extraConfig = ''
    Host nixos
      HostName nixos.tail174781.ts.net
      User lorenzo
      IdentityFile ~/.ssh/id_ed25519
  '';

  # Keys-only, matching the desktop. The local console still works if a key
  # is ever lost.
  services.openssh.extraConfig = ''
    PasswordAuthentication no
    KbdInteractiveAuthentication no
  '';

  homebrew = {
    enable = true;
    taps = [
      "nikitabobko/tap"
    ];
    casks = [
      "ghostty"
      "google-chrome"
      "mullvad-vpn"
      "nikitabobko/tap/aerospace"
      "obsidian"
      "signal"
      "spotify"
      "telegram"
      "whatsapp"
    ];
    brews = [
      "terminal-notifier"
    ];
    onActivation.cleanup = "zap";
  };

  users = {
    knownUsers = [ "lorenzo" ];
    users.lorenzo.uid = 501;
    users.lorenzo.shell = pkgs.fish;
    users.lorenzo.openssh.authorizedKeys.keys = [
      "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOKP7Z6l0U3ZQbmfyj6yAD62F16A/gkULyiOlG0fcEEL lorenzo@lorenzo-pc"
    ];
  };

  nixpkgs.hostPlatform = "aarch64-darwin";
}
