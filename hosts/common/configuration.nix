{ config, pkgs, ... }:

{
  nixpkgs.config.allowUnfree = true;

  nix.settings.experimental-features = [
    "nix-command"
    "flakes"
  ];

  environment.systemPackages = with pkgs; [
    atuin
    bash-language-server
    bat
    bitcoin
    cargo-nextest
    diesel-cli
    direnv
    docker
    eza
    gh
    gnupg
    google-chrome
    grpcurl
    jq
    lua-language-server
    neovim
    ngrok
    nil
    nixfmt-rfc-style
    openssl
    pkg-config
    postgresql
    postman
    protobuf
    ripgrep
    rustup
    signal-desktop
    slack
    spotify
    sql-formatter
    starship
    stow
    svelte-language-server
    tailscale
    telegram-desktop
    tree-sitter
    yaml-language-server
    zellij
    zoxide
  ];

  fonts.packages = [
    pkgs.nerd-fonts.fira-code
  ];

  environment.variables = {
    EDITOR = "nvim";
    PKG_CONFIG_PATH = "${pkgs.postgresql}/lib/pkgconfig";
    LIBRARY_PATH = "${pkgs.postgresql.lib}/lib";
  };

  programs.fish.enable = true;

  services.openssh.enable = true;
  services.tailscale.enable = true;
}
