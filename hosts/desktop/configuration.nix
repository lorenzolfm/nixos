{ config, pkgs, ... }:

{
  imports = [
    ../common/configuration.nix
    ./hardware-configuration.nix
    ./backup.nix
    ./scb-repo.nix
    ./claude-tray.nix
    ./claude-ps.nix
    ./rgb.nix
  ];

  nixpkgs.overlays = [
    (final: _prev: { sparrow = final.callPackage ../../pkgs/sparrow/package.nix { }; })
    # librepods decides whether the AirPods are the active output by substring
    # matching the default sink's name. WirePlumber 0.5.13 changed how it formats
    # those names, so the match silently fails and ear detection stops pausing
    # media -- the events still fire, the pause is just never reached.
    # 0001 is upstream PR #417 (open, unmerged), comparing the bluez
    # `device.string` MAC exactly instead. 0002 guards a NULL deref that PR
    # leaves in its new callback. Drop both once #417 reaches nixpkgs.
    #
    # 0003 is local. On both pods out librepods set the card profile to "off",
    # destroying the sink, so the default sink fell back to another device and
    # on re-insertion the MAC read back as garbage -- resume was unreachable
    # and taking the AirPods off killed playback rather than pausing it. It
    # also gated the pause on a flaky blocking D-Bus read that left the resume
    # list empty. Not filed upstream yet.
    #
    # 0004 is local. Chromium exports the MPRIS Player interface with no
    # introspection XML, and QDBusInterface resolves properties through that
    # metadata, so every PlaybackStatus read came back empty and librepods
    # concluded nothing was playing -- ear detection paused nothing at all.
    # Reads org.freedesktop.DBus.Properties directly instead, as playerctl
    # does. Not filed upstream yet.
    (_final: prev: {
      librepods = prev.librepods.overrideAttrs (old: {
        patches = (old.patches or [ ]) ++ [
          ../../pkgs/librepods/0001-match-sinks-by-mac-not-name.patch
          ../../pkgs/librepods/0002-guard-null-sink-info.patch
          ../../pkgs/librepods/0003-pause-on-ear-removal-without-tearing-down-sink.patch
          ../../pkgs/librepods/0004-read-mpris-properties-without-introspection.patch
        ];
        # The patches are rooted at the repo, but sourceRoot is source/linux.
        patchFlags = [ "-p2" ];
      });
    })
    # Ghostty's GTK frontend pulses the OSC 9;4 indeterminate progress bar once
    # per progress report, and GtkProgressBar paces its animation by the gap
    # between pulse() calls -- so the bar's speed is whatever rate the program
    # in the terminal happens to emit at. Claude Code reports about once a
    # second, which moves the block 10% per second: ten seconds to cross, and a
    # stall whenever reports pause. The macOS frontend animates its own 1.2s
    # bounce and ignores the report rate, which is why this only looks wrong on
    # Linux. 0001 drives the pulse from a 120ms timer instead. Not filed
    # upstream yet.
    (_final: prev: {
      ghostty = prev.ghostty.overrideAttrs (old: {
        patches = (old.patches or [ ]) ++ [
          ../../pkgs/ghostty/0001-pulse-progress-bar-on-a-steady-timer.patch
        ];
      });
    })
  ];

  boot.loader.systemd-boot.enable = true;
  boot.loader.systemd-boot.configurationLimit = 5;
  boot.loader.efi.canTouchEfiVariables = true;

  services.earlyoom = {
    enable = true;
    freeMemThreshold = 5;
    freeSwapThreshold = 10;
    extraArgs = [
      "--sort-by-rss"
      "--avoid"
      "(^systemd|Hyprland|gnome-shell|gnome-keyring|^gdm|pipewire|wireplumber|dbus-broker|dconf|xdg-|at-spi|gvfsd|swaync|waybar|portal)"
      "--prefer"
      "(chrome|rust-analyzer)"
    ];
  };

  zramSwap.enable = true;

  networking = {
    hostName = "nixos";
    extraHosts = ''
      10.0.1.1 homelab.local
      10.0.1.9 homelab-1.local
      10.0.1.4 homelab-2.local
    '';
    networkmanager.enable = true;
    nftables.enable = true;
    firewall = {
      enable = true;
      logReversePathDrops = true;
      logRefusedConnections = true;
      interfaces = {
        tailscale0 = {
          allowedTCPPorts = [ 22 ];
        };
        "br-*" = {
          allowedTCPPorts = [ 8000 ];
        };
      };
    };
  };

  time.timeZone = "America/Sao_Paulo";
  i18n.defaultLocale = "en_US.UTF-8";

  i18n.extraLocaleSettings = {
    LC_ADDRESS = "en_US.UTF-8";
    LC_IDENTIFICATION = "en_US.UTF-8";
    LC_MEASUREMENT = "en_US.UTF-8";
    LC_MONETARY = "en_US.UTF-8";
    LC_NAME = "en_US.UTF-8";
    LC_NUMERIC = "en_US.UTF-8";
    LC_PAPER = "en_US.UTF-8";
    LC_TELEPHONE = "en_US.UTF-8";
    LC_TIME = "en_US.UTF-8";
  };

  services.xserver.enable = true;
  services.xserver.displayManager.gdm.enable = true;
  # Hyprland installs two sessions; the plain one bypasses UWSM, so
  # graphical-session.target never activates and the portals stay dead
  # (GTK apps then ignore color-scheme and render light). Pre-select the
  # uwsm-managed session so a login can't silently land on the broken one.
  services.displayManager.defaultSession = "hyprland-uwsm";
  services.xserver.desktopManager.gnome.enable = true;
  services.xserver.xkb = {
    layout = "us";
    variant = "";
  };

  services.printing.enable = true;

  services.pulseaudio.enable = false;
  security.rtkit.enable = true;
  services.pipewire = {
    enable = true;
    alsa.enable = true;
    alsa.support32Bit = true;
    pulse.enable = true;
    jack.enable = true;
    # The AirPods' own play/pause button sends an AVRCP passthrough command,
    # which only reaches the desktop if something registers a player with
    # bluez and forwards to MPRIS. Two things can register one and only one
    # may, or the registration is refused and the button does nothing.
    #
    # WirePlumber's dummy player registers but forwards nothing, so it is
    # explicitly off; mpris-proxy below does the forwarding and is what
    # actually makes the button work. Verified by testing both in isolation.
    wireplumber.extraConfig."51-bluez-avrcp" = {
      "monitor.bluez.properties" = {
        "bluez5.dummy-avrcp-player" = false;
      };
    };
  };

  # Bridges AVRCP passthrough from the AirPods to MPRIS, so the button on the
  # headphones pauses whatever is playing. bluez ships the unit; asDropin
  # enables it without redefining it (a full definition would collide with
  # the unit bluez already installs at the same path).
  systemd.user.services.mpris-proxy = {
    overrideStrategy = "asDropin";
    wantedBy = [ "default.target" ];
  };

  users.users.lorenzo = {
    isNormalUser = true;
    description = "Lorenzo";
    openssh.authorizedKeys.keys = [
      "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAID/67UYdIgV7PwpayA/4Ogc7u84q8FQ5AKrLLRX7q3zT lorenzo@lorenzo-mac"
    ];
    extraGroups = [
      "networkmanager"
      "wheel"
      "docker"
    ];
    packages = with pkgs; [
      cliphist
      cmatrix
      fd
      ffmpeg
      file
      fzf
      slack
      grim
      pavucontrol
      pinentry-tty
      poppler
      slurp
      sparrow
      swappy
      vlc
      wbg
      wl-clipboard
      yazi
    ];
  };

  # Qt defaults to the xcb platform plugin and aborts under Hyprland, where
  # there is no X display. Prefer wayland, keeping xcb as fallback for Qt
  # apps built without the wayland plugin.
  environment.sessionVariables.QT_QPA_PLATFORM = "wayland;xcb";

  environment.systemPackages = with pkgs; [
    (appimage-run.override {
      extraPkgs = pkgs: [ pkgs.xorg.libxshmfence ];
    })
    bitcoin
    boost
    cmake
    discord
    docker
    gcc
    ghostty
    gnumake
    google-chrome
    gws
    heaptrack
    hyperfine
    jellyfin-desktop
    libevent
    libnotify
    librepods
    libsystemtap
    linuxPackages.perf
    obs-studio
    obsidian
    pamixer
    pkgconf
    playerctl
    protols
    python314
    qrencode
    rofi
    signal-desktop
    spotify
    sqlite
    ssss
    swaynotificationcenter
    telegram-desktop
    terraform
    trezor-suite
    vicinae
    waybar
    zbar
    zmqpp
  ];

  boot.kernel.sysctl."kernel.perf_event_paranoid" = 1;
  boot.kernel.sysctl."kernel.kptr_restrict" = 0;

  virtualisation.docker.enable = true;
  users.defaultUserShell = pkgs.fish;

  programs.git = {
    enable = true;
    config = {
      user = {
        signingKey = "/home/lorenzo/.ssh/id_ed25519.pub";
        email = "maturanolorenzo@gmail.com";
        name = "Lorenzo";
      };
      gpg = {
        format = "ssh";
        ssh = {
          allowedSignersFile = "/home/lorenzo/.ssh/allowed-signers";
        };
      };
      commit = {
        gpgSign = true;
      };
    };
  };

  programs.hyprland = {
    enable = true;
    xwayland.enable = true;
    # Without UWSM nothing activates graphical-session.target, so
    # xdg-desktop-portal (Requisite=graphical-session.target) can never
    # start: no portals, and GTK apps ignore the dark color-scheme.
    withUWSM = true;
  };

  services.fail2ban = {
    enable = true;
    # Never ban trusted networks: loopback, LAN, and the Tailscale
    # CGNAT range (100.64.0.0/10) so my notebook over Tailscale is exempt.
    ignoreIP = [
      "127.0.0.1/8"
      "10.0.0.0/8"
      "100.64.0.0/10"
    ];
  };
  services.blueman.enable = true;

  services.xserver.videoDrivers = [ "nvidia" ];
  hardware = {
    enableAllFirmware = true;

    graphics.enable = true;

    nvidia = {
      modesetting.enable = true;
      open = false;
      nvidiaSettings = true;
    };

    bluetooth = {
      enable = true;
      powerOnBoot = true;
      settings = {
        General = {
          Experimental = true;
          FastConnectable = true;
          Class = "0x000100";
          DiscoverableTimeout = 0;
          PairableTimeout = 0;
        };
        Policy = {
          AutoEnable = true;
          ReconnectAttempts = 0;
        };
      };
    };
  };

  xdg.portal = {
    enable = true;
    extraPortals = [ pkgs.xdg-desktop-portal-gtk ];
    xdgOpenUsePortal = true;
  };

  programs.hyprland.portalPackage = pkgs.xdg-desktop-portal-hyprland;

  system.stateVersion = "24.11";
}
