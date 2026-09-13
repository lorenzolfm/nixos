{
  pkgs,
  claude-ps,
  ...
}:

{
  environment.systemPackages = [
    claude-ps.packages.${pkgs.stdenv.hostPlatform.system}.default
  ];
}
