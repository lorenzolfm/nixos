{
  pkgs,
  luneta,
  ...
}:

{
  environment.systemPackages = [
    # Both `luneta` and `lunetad`.
    luneta.packages.${pkgs.stdenv.hostPlatform.system}.lunetad
  ];
}
