{ config, pkgs, ... }:

{
  imports = [
    ../common/configuration.nix
    ./hardware-configuration.nix
  ];

  boot.loader.systemd-boot.enable = true;
  boot.loader.efi.canTouchEfiVariables = true;

  networking = {
    hostName = "nixos";
    extraHosts = ''
      10.0.1.1 homelab.local
      10.0.1.9 homelab-1.local
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
    extraGroups = [
      "networkmanager"
      "wheel"
      "docker"
    ];
    packages = with pkgs; [
      bitwarden-desktop
      cliphist
      cmatrix
      discord
      grim
      kubectl
      nodejs_23
      pavucontrol
      pinentry-tty
      slurp
      sparrow
      swappy
      wbg
      wl-clipboard
    ];
  };

  environment.systemPackages = with pkgs; [
    appimage-run
    ghostty
    libnotify
    pamixer
    playerctl
    protols
    rofi-wayland
    swaynotificationcenter
    waybar
  ];

  virtualisation.docker.enable = true;
  users.defaultUserShell = pkgs.fish;

  programs.git = {
    enable = true;
    config = {
      user = {
        signingKey = "/home/lorenzo/.ssh/id_rsa.pub";
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

  services.fail2ban.enable = true;

  services.xserver.videoDrivers = [ "nvidia" ];
  hardware = {
    enableAllFirmware = true;

    graphics.enable = true;

    nvidia = {
      modesetting.enable = true;
      open = false;
      nvidiaSettings = true;
    };
  };

  xdg.portal.enable = true;
  xdg.portal.extraPortals = [ pkgs.xdg-desktop-portal-gtk ];

  system.stateVersion = "24.11";
}
