{ config, pkgs, ... }:

{
  imports = [
    ../common/configuration.nix
    ./hardware-configuration.nix
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
        enp4s0 = {
          allowedTCPPorts = [ 9000 ];
        };
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
    libsystemtap
    linuxPackages.perf
    obs-studio
    pamixer
    pkgconf
    playerctl
    protols
    python314
    rofi
    signal-desktop
    spotify
    sqlite
    terraform
    swaynotificationcenter
    telegram-desktop
    vicinae
    waybar
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
