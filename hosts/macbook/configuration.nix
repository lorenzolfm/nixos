{ config, pkgs, ... }:

{
  nix.settings.experimental-features = "nix-command flakes";
  nixpkgs.config.allowUnfree = true;

  # System packages
  environment.systemPackages = with pkgs; [
    atuin
    diesel-cli
    direnv
    docker
    fish
    google-chrome
    neovim
    nil
    postman
    protobuf
    raycast
    rustup
    slack
    starship
    stow
    tailscale
    zellij
    zoxide
  ];

  # Environment variables
  environment.variables = {
    EDITOR = "nvim";
    PKG_CONFIG_PATH = "${pkgs.postgresql}/lib/pkgconfig";
    LIBRARY_PATH = "${pkgs.postgresql.lib}/lib";
  };

  # Fish shell configuration
  programs.fish.enable = true;

  # User configuration
  users.users.lorenzo = {
    home = "/Users/lorenzo";
    shell = pkgs.fish;
  };

  # System services
  services.openssh.enable = true;
  services.tailscale.enable = true;

  # System version (don't change unless you know what you're doing)
  system.stateVersion = 6;

  # Platform configuration
  nixpkgs.hostPlatform = "aarch64-darwin";
}
