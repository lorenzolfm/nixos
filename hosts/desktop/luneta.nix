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

  # The zellij plugin, built from the same commit. ~/.config/zellij/config.kdl loads it from here.
  environment.etc."zellij/plugins/luneta.wasm".source =
    "${luneta.packages.${pkgs.stdenv.hostPlatform.system}.luneta}/luneta.wasm";
}
