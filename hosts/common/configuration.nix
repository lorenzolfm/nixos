{
  pkgs,
  claude-code,
  llm-agents,
  rust-overlay,
  wt,
  ...
}:
{
  imports = [
    ./claude-ps.nix
    ./lune.nix
  ];

  nixpkgs.config.allowUnfree = true;
  nixpkgs.overlays = [ rust-overlay.overlays.default ];

  nix.settings = {
    experimental-features = [
      "nix-command"
      "flakes"
    ];
    extra-substituters = [
      # Prebuilt llm-agents packages (antigravity-cli, codex, pi).
      "https://cache.numtide.com"
      # The homelab's attic cache (lorenzolfm/homelab#431): lune and elo
      # builds pushed by CI and by cache-push (./attic.nix). It resolves only
      # on the tailnet. It reports priority 41, so cache.nixos.org (40) is
      # still asked first.
      "https://attic.tail174781.ts.net/main"
    ];
    extra-trusted-public-keys = [
      "niks3.numtide.com-1:DTx8wZduET09hRmMtKdQDxNNthLQETkc/yaX7M4qK0g="
      "main:4Zt9CH+9bGa5vSsFCdxOmqGjhItSREekmxJaB9wmgYY="
    ];
    # Off the tailnet (the Mac on the road) attic does not resolve. These two
    # cap that at about 5 s, after which the build runs locally instead of
    # failing. They apply to every substituter, not only attic.
    connect-timeout = 5;
    fallback = true;
  };

  environment.systemPackages = with pkgs; [
    atuin
    bash-language-server
    bat
    bitwarden-cli
    bottom
    (rust-bin.stable.latest.default.override {
      extensions = [
        "rust-src"
        "rust-analyzer"
      ];
    })
    cargo-nextest
    claude-code.packages.${pkgs.stdenv.hostPlatform.system}.claude-code
    diesel-cli
    direnv
    discord
    (callPackage ../../pkgs/ephyra-cli { })
    eza
    gh
    # A real git ahead of /usr/bin/git on macOS. That one is Apple's xcrun shim,
    # and inside a Nix dev shell DEVELOPER_DIR points it at the Nix apple-sdk,
    # which has no git, so it fails with "tool 'git' not found".
    git
    # nixpkgs only builds ghostty on Linux; macOS gets the upstream signed app.
    (if stdenv.hostPlatform.isDarwin then ghostty-bin else ghostty)
    gnupg
    google-chrome
    jq
    kubectl
    kubeseal
    llm-agents.packages.${pkgs.stdenv.hostPlatform.system}.antigravity-cli
    llm-agents.packages.${pkgs.stdenv.hostPlatform.system}.codex
    llm-agents.packages.${pkgs.stdenv.hostPlatform.system}.pi
    lua-language-server
    neovim
    nil
    nixfmt
    nodejs_24
    obsidian
    openssl
    pkg-config
    ripgrep
    rust-script
    sql-formatter
    starship
    stow
    svelte-language-server
    tailscale
    tree-sitter
    typescript-language-server
    vscode-langservers-extracted
    wt.packages.${pkgs.stdenv.hostPlatform.system}.default
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
