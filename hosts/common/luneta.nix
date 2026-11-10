{
  pkgs,
  luneta,
  luneta-unstable,
  ...
}:

let
  system = pkgs.stdenv.hostPlatform.system;

  # The release pinned in flake.nix, as `luneta-stable` and `lunetad-stable`.
  # They are symlinks into the pinned store path. luneta starts the lunetad
  # beside its resolved binary, so `luneta-stable` starts the pinned daemon and
  # not the tip's. The fish hook is left out, as the tip's package installs it.
  pinned = luneta.packages.${system}.lunetad;
  lunetaStable = pkgs.runCommand "luneta-stable-${pinned.version}" { } ''
    mkdir -p $out/bin
    ln -s ${pinned}/bin/luneta $out/bin/luneta-stable
    ln -s ${pinned}/bin/lunetad $out/bin/lunetad-stable
  '';
in
{
  environment.systemPackages = [
    # Both `luneta` and `lunetad`, at luneta's main.
    luneta-unstable.packages.${system}.lunetad
    lunetaStable
  ];
}
