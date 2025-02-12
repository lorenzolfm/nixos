# Edit this configuration file to define what should be installed on
# your system.  Help is available in the configuration.nix(5) man page
# and in the NixOS manual (accessible by running ‘nixos-help’).

{ config, pkgs, ... }:

{
  imports =
    [ # Include the results of the hardware scan.
      ./hardware-configuration.nix
    ];

  # Bootloader.
  boot.loader.systemd-boot.enable = true;
  boot.loader.efi.canTouchEfiVariables = true;

  networking.hostName = "nixos"; # Define your hostname.
  networking.extraHosts =
  ''
    10.0.1.1 homelab.local
    10.0.1.9 homelab-1.local
  '';

  # Configure network proxy if necessary
  # networking.proxy.default = "http://user:password@proxy:port/";
  # networking.proxy.noProxy = "127.0.0.1,localhost,internal.domain";

  # Enable networking
  networking.networkmanager.enable = true;

  # Set your time zone.
  time.timeZone = "America/Sao_Paulo";

  # Select internationalisation properties.
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

  # Enable the X11 windowing system.
  services.xserver.enable = true;

  # Enable the GNOME Desktop Environment.
  services.xserver.displayManager.gdm.enable = true;
  services.xserver.desktopManager.gnome.enable = true;

  # Configure keymap in X11
  services.xserver.xkb = {
    layout = "us";
    variant = "";
  };

  # Enable CUPS to print documents.
  services.printing.enable = true;

  # Enable sound with pipewire.
  hardware.pulseaudio.enable = false;
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
    extraGroups = [ "networkmanager" "wheel" "docker" ];
    packages = with pkgs; [
      atuin
      bat
      bitwarden-desktop
      cliphist
      cmatrix
      diesel-cli
      docker
      eza
      gcc
      gh
      gnupg
      google-chrome
      grim
      grpcurl
      kubectl
      nil
      nodejs_23
      pavucontrol
      pinentry-tty
      postman
      protobuf
      rustup
      slack
      slurp
      spotify
      swappy
      telegram-desktop
      wbg
      wl-clipboard
      zellij
      zoxide
    ];
  };

  nixpkgs.config.allowUnfree = true;

  environment.systemPackages = with pkgs; [
    bash-language-server
    bitcoin
    cargo-nextest
    direnv
    ghostty
    jq
    libnotify
    lua-language-server
    neovim
    ngrok
    openssl
    pamixer
    pkg-config
    playerctl
    postgresql
    protols
    ripgrep
    rofi-wayland
    starship
    stow
    svelte-language-server
    swaynotificationcenter
    tree-sitter
    waybar
    yaml-language-server
  ];

  virtualisation.docker.enable = true;
  programs.fish.enable = true;
  users.defaultUserShell = pkgs.fish;

  environment.variables = {
    EDITOR = "nvim";
    PKG_CONFIG_PATH = "${pkgs.postgresql}/lib/pkgconfig";
    LIBRARY_PATH = "${pkgs.postgresql.lib}/lib";
  };

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

  fonts.packages = with pkgs; [
    (nerdfonts.override { fonts = [ "FiraCode" ]; })
    fira-code
  ];

  services.xserver.videoDrivers = ["nvidia"];
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
  # Some programs need SUID wrappers, can be configured further or are
  # started in user sessions.
  # programs.mtr.enable = true;
  # programs.gnupg.agent = {
  #   enable = true;
  #   enableSSHSupport = true;
  # };

  # List services that you want to enable:

  # Enable the OpenSSH daemon.
  # services.openssh.enable = true;

  # Open ports in the firewall.
  # networking.firewall.allowedTCPPorts = [ ... ];
  # networking.firewall.allowedUDPPorts = [ ... ];
  # Or disable the firewall altogether.
  # networking.firewall.enable = false;

  # This value determines the NixOS release from which the default
  # settings for stateful data, like file locations and database versions
  # on your system were taken. It‘s perfectly fine and recommended to leave
  # this value at the release version of the first install of this system.
  # Before changing this value read the documentation for this option
  # (e.g. man configuration.nix or on https://nixos.org/nixos/options.html).
  system.stateVersion = "24.11"; # Did you read the comment?

}
