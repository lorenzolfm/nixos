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
    #
    # Extra args go before "run"'s app name only as numes options; pi flags such
    # as --provider/--model must come after "pi", so "run pi" stays mid-line
    # and user args are appended. The default model pins what numes injects into
    # pi's command line; without it numes picks its own default model.
    (pkgs.writeShellScriptBin "numes-pi" ''
      set -euo pipefail
      export NUMES_API_KEY="$(cat ${config.sops.secrets."numes-api-key".path})"
      # numes gives pi its own agent dir, so pi misses the luneta extension that
      # lunetad keeps current in ~/.pi; without it these agents are invisible to
      # luneta. Link the one file: the rest of ~/.pi's extensions are not for
      # numes. Done here, as the user, because tmpfiles would make any missing
      # parent dirs root-owned.
      luneta_ext="$HOME/.pi/agent/extensions/luneta.ts"
      if [ -e "$luneta_ext" ]; then
        mkdir -p "$HOME/.local/share/numes/apps/pi/extensions"
        ln -sfn "$luneta_ext" "$HOME/.local/share/numes/apps/pi/extensions/luneta.ts"
      fi
      exec ${lib.getExe numes} run pi \
        --provider numes-org-cashu-devs \
        --model z-ai/glm-5.3:org-cashu-devs \
        "$@"
    '')
  ];
}
