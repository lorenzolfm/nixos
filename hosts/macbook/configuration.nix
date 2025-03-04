{ config, pkgs, ... }:

{
  nix.settings.experimental-features = "nix-command flakes";
  nixpkgs.config.allowUnfree = true;

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
    starship
    stow
    tailscale
    zellij
    zoxide
  ];

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

  programs.fish.enable = true;

  users.users.lorenzo = {
    home = "/Users/lorenzo";
    shell = pkgs.fish;
  };

  services.openssh.enable = true;
  services.tailscale.enable = true;

  system.stateVersion = 6;

  nixpkgs.hostPlatform = "aarch64-darwin";
}
