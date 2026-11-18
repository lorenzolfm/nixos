{
  pkgs,
  luneta,
  ...
}:

{
  # Both `luneta` and `lunetad`, at luneta's main.
  environment.systemPackages = [
    luneta.packages.${pkgs.stdenv.hostPlatform.system}.lunetad
  ];
}
