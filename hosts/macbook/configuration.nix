{ config, pkgs, ... }:

{
  nix.settings.experimental-features = "nix-command flakes";
  nixpkgs.config.allowUnfree = true;

  system.defaults = {
    dock.autohide = true;
    finder.FXPreferredViewStyle = "clmv";
    NSGlobalDomain.AppleICUForce24HourTime = true;
    NSGlobalDomain.AppleInterfaceStyle = "Dark";
    NSGlobalDomain.KeyRepeat = 2;
  };

  environment.systemPackages = with pkgs; [
    atuin
    diesel-cli
    direnv
    docker
    fish
    google-chrome
    neovim
    nil
    nixfmt-rfc-style
    postman
    protobuf
    raycast
    rustup
    slack
    spotify
    starship
    stow
    tailscale
    zellij
    zoxide
  ];

  homebrew = {
    enable = true;
    casks = [
      "ghostty"
    ];
    onActivation.cleanup = "zap";
  };

  fonts.packages = [
    pkgs.nerd-fonts.fira-code
  ];

  programs.fish.enable = true;
  users.knownUsers = [ "lorenzo" ];
  users.users.lorenzo.uid = 501;
  users.users.lorenzo.shell = pkgs.fish;

  environment.variables = {
    EDITOR = "nvim";
    PKG_CONFIG_PATH = "${pkgs.postgresql}/lib/pkgconfig";
    LIBRARY_PATH = "${pkgs.postgresql.lib}/lib";
  };

  environment.etc."gitconfig".text = ''
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

  services.openssh.enable = true;
  services.tailscale.enable = true;

  system.stateVersion = 6;

  nixpkgs.hostPlatform = "aarch64-darwin";
}
