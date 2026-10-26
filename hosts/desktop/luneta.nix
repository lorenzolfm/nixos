{
  pkgs,
  luneta,
  ...
}:

{
  environment.systemPackages = [
    # Both `luneta` and `lunetad`. `packages.luneta` is the zellij plugin's .wasm.
    luneta.packages.${pkgs.stdenv.hostPlatform.system}.lunetad
  ];
}
