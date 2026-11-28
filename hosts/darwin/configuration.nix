# Shared by every Mac (macbook, work-macbook). What only one Mac has goes in
# that host's configuration.nix, together with its networking names.
{ config, pkgs, ... }:

{
  imports = [
    ../common/configuration.nix
    ./disk-cleanup.nix
    ./display-arrangement.nix
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
        ];
        autohide = true;
        show-recents = false;
        show-process-indicators = false;
      };
      finder.FXPreferredViewStyle = "clmv";
      screensaver.askForPasswordDelay = 10;
      NSGlobalDomain = {
        AppleICUForce24HourTime = true;
        AppleInterfaceStyle = "Dark";
        KeyRepeat = 2;
      };
    };
    stateVersion = 6;
  };

  # Minutes of inactivity, same on battery and charger. nix-darwin applies this
  # through systemsetup with its output silenced, so check `pmset -g custom`.
  power.sleep = {
    display = 30;
    computer = 60;
  };

  # hidutil's mapping is lost on reboot, and nix-darwin's boot daemon
  # (activate-system) never reapplies it: its App Management check fails outside
  # a GUI session and aborts the script before the keyboard step. So do it at
  # login instead.
  launchd.user.agents.keyboard-remap.serviceConfig = {
    ProgramArguments = [
      "/usr/bin/hidutil"
      "property"
      "--set"
      (builtins.toJSON { UserKeyMapping = config.system.keyboard.userKeyMapping; })
    ];
    RunAtLoad = true;
  };

  environment = {
    systemPackages = with pkgs; [
      aerospace
      pinentry_mac
      raycast
      terminal-notifier
      # Nix's git reads its system config from its own store path, not
      # /etc/gitconfig (only Apple's /usr/bin/git does). Bake the path into
      # the binary rather than a session variable: shells, tmux and GUI apps
      # started before a switch never see a new variable, and commit as
      # user@hostname. hiPrio wins over the plain git from common.
      (lib.hiPrio (symlinkJoin {
        name = "git-etc-gitconfig";
        paths = [ git ];
        nativeBuildInputs = [ makeWrapper ];
        postBuild = ''
          wrapProgram $out/bin/git --set-default GIT_CONFIG_SYSTEM /etc/gitconfig
        '';
      }))
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

      # The git wrapper's GIT_CONFIG_SYSTEM replaces the store gitconfig that
      # set this.
      [credential]
      helper = osxkeychain
    '';
  };

  # Keys-only, matching the desktop. The local console still works if a key
  # is ever lost.
  services.openssh.extraConfig = ''
    PasswordAuthentication no
    KbdInteractiveAuthentication no
  '';

  # Casks are per host: only what nixpkgs can't provide.
  homebrew = {
    enable = true;
    # Not "zap": zapping a cask that moves to nixpkgs deletes the app's data
    # too, e.g. ~/Library/Application Support/Google/Chrome for google-chrome.
    onActivation.cleanup = "uninstall";
  };

  # Not homebrew.masApps: with any entry there, onActivation.cleanup uninstalls
  # every App Store app missing from that list. mas can only install apps the
  # Apple Account already has, so get Xcode once in the App Store first.
  programs.mas = {
    enable = true;
    # The default runs `mas update` on every rebuild.
    update = false;
    packages.Xcode = 497799835;
  };

  users = {
    knownUsers = [ "lorenzo" ];
    users.lorenzo = {
      uid = 501;
      shell = pkgs.fish;
    };
  };

  nixpkgs.hostPlatform = "aarch64-darwin";
}
