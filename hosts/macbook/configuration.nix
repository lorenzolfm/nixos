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
          "/Applications/Nix Apps/Ghostty.app"
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

      # U.S. is the only keyboard layout. Ghostty takes Option for Alt only
      # on the U.S. layouts, and luneta's Alt keys (Alt r dictates) need it.
      # The other two sources are macOS's own: the emoji palette and the
      # accents a held key offers.
      CustomUserPreferences."com.apple.HIToolbox" =
        let
          us = {
            InputSourceKind = "Keyboard Layout";
            "KeyboardLayout ID" = 0;
            "KeyboardLayout Name" = "U.S.";
          };
          palette = {
            "Bundle ID" = "com.apple.CharacterPaletteIM";
            InputSourceKind = "Non Keyboard Input Method";
          };
          pressAndHold = {
            "Bundle ID" = "com.apple.PressAndHold";
            InputSourceKind = "Non Keyboard Input Method";
          };
        in
        {
          AppleEnabledInputSources = [
            palette
            pressAndHold
            us
          ];
          AppleSelectedInputSources = [
            pressAndHold
            us
          ];
        };
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
      aerospace
      pinentry_mac
      raycast
      terminal-notifier
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

  # Only what nixpkgs can't provide yet. mullvad-vpn has no macOS build there,
  # telegram is the native macOS app (nixpkgs has only Telegram Desktop), and
  # signal, spotify and whatsapp are behind the cask versions: installing them
  # from nixpkgs would downgrade them.
  homebrew = {
    enable = true;
    casks = [
      "mullvad-vpn"
      "signal"
      "spotify"
      "telegram"
      "whatsapp"
    ];
    # Not "zap": zapping a cask that moves to nixpkgs deletes the app's data
    # too, e.g. ~/Library/Application Support/Google/Chrome for google-chrome.
    onActivation.cleanup = "uninstall";
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
