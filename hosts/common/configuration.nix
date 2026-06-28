{
  config,
  pkgs,
  claude-code,
  rust-overlay,
  ...
}:
{
  nixpkgs.config.allowUnfree = true;
  nixpkgs.overlays = [ rust-overlay.overlays.default ];

  nix.settings.experimental-features = [
    "nix-command"
    "flakes"
  ];

  environment.systemPackages = with pkgs; [
    atuin
    bash-language-server
    bat
    bottom
    (rust-bin.stable.latest.default.override {
      extensions = [
        "rust-src"
        "rust-analyzer"
      ];
    })
    cargo-nextest
    claude-code.packages.${pkgs.system}.claude-code
    codex
    diesel-cli
    direnv
    eza
    gh
    gnupg
    grpcurl
    jq
    k9s
    lua-language-server
    neovim
    ngrok
    nil
    nixfmt-rfc-style
    nodejs_24
    openssl
    pkg-config
    postgresql
    postman
    protobuf
    ripgrep
    rust-script
    sql-formatter
    sqlx-cli
    starship
    stow
    svelte-language-server
    tailscale
    telegram-desktop
    tree-sitter
    typescript-language-server
    vscode-langservers-extracted
    yaml-language-server
    yamlfmt
    zellij
    zoxide
  ];

  fonts.packages = [
    pkgs.nerd-fonts.fira-code
    pkgs.nerd-fonts.jetbrains-mono
  ];

  environment.variables = {
    EDITOR = "nvim";
    PKG_CONFIG_PATH = "${pkgs.openssl.dev}/lib/pkgconfig:${pkgs.postgresql}/lib/pkgconfig";
    LIBRARY_PATH = "${pkgs.postgresql.lib}/lib";
  };

  programs.fish.enable = true;

  services.openssh.enable = true;
  services.tailscale.enable = true;
}
