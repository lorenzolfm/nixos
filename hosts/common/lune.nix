{
  pkgs,
  lune,
  ...
}:

{
  # Both `lune` and `luned`, at lune's main.
  environment.systemPackages = [
    lune.packages.${pkgs.stdenv.hostPlatform.system}.luned
  ];
}
