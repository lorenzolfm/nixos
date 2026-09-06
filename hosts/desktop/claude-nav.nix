{
  pkgs,
  claude-nav,
  ...
}:

{
  environment.systemPackages = [
    claude-nav.packages.${pkgs.stdenv.hostPlatform.system}.default
  ];
}
