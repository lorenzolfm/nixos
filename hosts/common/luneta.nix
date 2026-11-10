{
  pkgs,
  luneta,
  luneta-unstable,
  ...
}:

let
  system = pkgs.stdenv.hostPlatform.system;

  # luneta's main, as `luneta-unstable` and `lunetad-unstable`. They are
  # symlinks into the tip's store path. luneta starts the lunetad beside its
  # resolved binary, so `luneta-unstable` starts the tip's daemon and not the
  # pinned one. The fish hook is left out, as the pinned package installs it.
  tip = luneta-unstable.packages.${system}.lunetad;
  lunetaUnstable = pkgs.runCommand "luneta-unstable-${tip.version}" { } ''
    mkdir -p $out/bin
    ln -s ${tip}/bin/luneta $out/bin/luneta-unstable
    ln -s ${tip}/bin/lunetad $out/bin/lunetad-unstable
  '';
in
{
  environment.systemPackages = [
    # Both `luneta` and `lunetad`, pinned to a release in flake.nix.
    luneta.packages.${system}.lunetad
    lunetaUnstable
  ];
}
