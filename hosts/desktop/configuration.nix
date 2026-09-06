{ config, pkgs, ... }:

{
  imports = [
    ../common/configuration.nix
    ./hardware-configuration.nix
    ./backup.nix
    ./scb-repo.nix
    ./claude-tray.nix
    ./claude-ps.nix
    ./claude-nav.nix
    ./oss-board.nix
    ./rgb.nix
  ];

  nixpkgs.overlays = [
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
      "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIC2msniVULYTITZN4q2LXHkN4AZV97ttv6hW507wuWB6 lorenzo@iphone-termius"
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

  # GTK 4.22 defaults to the Vulkan renderer, which here enumerates only the AMD
  # iGPU (card1) and never the NVIDIA card driving both monitors. GTK renders on
  # a GPU wired to no display and hands Hyprland a buffer it cannot sample, so
  # every GTK4 app (Files, gnome-text-editor) maps a solid black window. Use the
  # GL renderer instead.
  environment.sessionVariables.GSK_RENDERER = "ngl";

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

  # Port 22 is only open on tailscale0 (see networking.firewall above), so
  # reaching sshd already requires being on the tailnet. Keys-only on top of
  # that means a tailnet device alone is not enough to log in. The physical
  # console stays available if a key is ever lost.
  services.openssh.settings = {
    PasswordAuthentication = false;
    KbdInteractiveAuthentication = false;
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
