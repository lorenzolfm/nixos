# The personal Mac. Everything both Macs share is in ../darwin.
{
  imports = [
    ../darwin/configuration.nix
    ../common/attic.nix
    ./linux-builder.nix
    ./sops.nix
  ];

  # darwin-rebuild picks the flake config by LocalHostName, so this has to
  # match darwinConfigurations.macbook for a bare `darwin-rebuild switch`,
  # which reads the flake from /etc/nix-darwin -> ~/.config/nixos/main.
  networking = {
    hostName = "macbook";
    localHostName = "macbook";
    computerName = "macbook";
  };

  programs.ssh.extraConfig = ''
    Host nixos
      HostName nixos.tail174781.ts.net
      User lorenzo
      IdentityFile ~/.ssh/id_ed25519
  '';

  users.users.lorenzo.openssh.authorizedKeys.keys = [
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOKP7Z6l0U3ZQbmfyj6yAD62F16A/gkULyiOlG0fcEEL lorenzo@lorenzo-pc"
  ];

  # Only what nixpkgs can't provide yet. mullvad-vpn has no macOS build there,
  # obscura-vpn isn't packaged at all,
  # telegram is the native macOS app (nixpkgs has only Telegram Desktop), and
  # signal, spotify and whatsapp are behind the cask versions: installing them
  # from nixpkgs would downgrade them.
  homebrew.casks = [
    "mullvad-vpn"
    "obscura-vpn"
    "signal"
    "spotify"
    "telegram"
    "whatsapp"
  ];
}
