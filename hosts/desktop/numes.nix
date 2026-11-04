# Numes CLI, plus `numes-pi`: pi on Numes models with the key from sops.
{
  config,
  lib,
  pkgs,
  ...
}:
let
  numes = pkgs.callPackage ../../pkgs/numes { };
in
{
  sops.secrets."numes-api-key" = {
    owner = "lorenzo";
    mode = "0400";
  };

  environment.systemPackages = [
    numes
    # NUMES_API_KEY overrides the key numes would save under ~/.config/numes/apps
    # and is never written to disk. Set here rather than in the shell so only this
    # process tree sees it, and read at run time so it stays out of the store.
    (pkgs.writeShellScriptBin "numes-pi" ''
      set -euo pipefail
      export NUMES_API_KEY="$(cat ${config.sops.secrets."numes-api-key".path})"
      exec ${lib.getExe numes} run pi "$@"
    '')
  ];
}
