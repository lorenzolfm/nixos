{ config, pkgs, ... }:
let
  unstable =
    import
      (fetchTarball {
        url = "https://github.com/NixOS/nixpkgs/archive/nixpkgs-unstable.tar.gz";
        sha256 = "1h645aidj2cnbfmchl5rmvc1jhdwa19cr821939kwx2afmyl65lf";
      })
      {
        config = config.nixpkgs.config;
        system = config.nixpkgs.system;
      };
in
{
  nixpkgs.config.allowUnfree = true;

  nix.settings.experimental-features = [
    "nix-command"
    "flakes"
  ];

  nixpkgs.overlays = [
    (final: prev: {
      neovim = unstable.neovim;
    })
  ];

  environment.systemPackages = with pkgs; [
    atuin
    bash-language-server
    bat
    bitcoin
    bottom
    cargo-nextest
    diesel-cli
    direnv
    discord
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
    yamlfmt
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
