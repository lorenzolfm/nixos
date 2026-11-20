# `cache-push [flake]`: build a flake's checks and packages, then push them to
# the homelab's attic cache (lorenzolfm/homelab#431) so CI and the Mac can pull
# them instead of building. Run it on purpose, from luneta, elo or another
# public flake; nothing pushes automatically. The Mac only pulls (see
# hosts/common).
#
# Public flakes only: cache `main` is world-readable on the tailnet, and the
# push includes the flake's cleaned source.
{
  config,
  lib,
  pkgs,
  ...
}:
let
  system = pkgs.stdenv.hostPlatform.system;

  # Nothing secret in here: attic reads the token from the sops file at run
  # time, so the token never reaches the store or ~/.config. The wrapper points
  # XDG_CONFIG_HOME at this directory for attic alone, which also leaves any
  # ~/.config/attic the user made by hand untouched.
  atticConfigHome = pkgs.writeTextDir "attic/config.toml" ''
    default-server = "homelab"

    [servers.homelab]
    endpoint = "https://attic.tail174781.ts.net/"
    token-file = "${config.sops.secrets."attic-push-token".path}"
  '';
in
{
  # Push token, sub `desktop`, pull+push on `main` only. It lasts a year; the
  # rotation is tracked in lorenzolfm/homelab#431.
  sops.secrets."attic-push-token" = {
    owner = "lorenzo";
    mode = "0400";
  };

  environment.systemPackages = [
    (pkgs.writeShellApplication {
      name = "cache-push";
      runtimeInputs = [
        pkgs.attic-client
        pkgs.jq
      ];
      text = ''
        flake="''${1:-.}"
        system=${lib.escapeShellArg system}

        err="$(mktemp)"
        trap 'rm -f "$err"' EXIT

        # Prints the flake's installables under $1 (checks or packages). A flake
        # without that output is fine; any other eval error stops the push.
        installables_of() {
          local names
          if names="$(nix eval --json "$flake#$1.$system" --apply builtins.attrNames 2>"$err")"; then
            # Keep warnings such as "Git tree is dirty" visible.
            cat "$err" >&2
            jq -r --arg prefix "$flake#$1.$system" '.[] | "\($prefix).\"\(.)\""' <<<"$names"
          elif grep -q "does not provide attribute" "$err"; then
            return 0
          else
            cat "$err" >&2
            return 1
          fi
        }

        installables=()
        for output in checks packages; do
          # A plain assignment, so set -e stops on an eval error; a process
          # substitution would hide it.
          found="$(installables_of "$output")"
          if [ -n "$found" ]; then
            mapfile -t -O "''${#installables[@]}" installables <<<"$found"
          fi
        done
        if [ "''${#installables[@]}" -eq 0 ]; then
          echo "cache-push: $flake has no checks or packages for $system" >&2
          exit 1
        fi

        # Evaluate once and use these derivations for both the build and the
        # push. Evaluating again after a long build would see any edit made
        # meanwhile (or a moved github: ref) and push a closure whose outputs
        # were never built.
        drvs_out="$(nix path-info --derivation "''${installables[@]}")"
        mapfile -t drvs <<<"$drvs_out"

        echo "cache-push: building ''${#drvs[@]} derivations of $flake" >&2
        nix build --no-link "''${drvs[@]/%/^*}"

        # Push the whole build closure, not only the results: crane's deps-only
        # artifacts, the toolchain and the sources are what save CI the most
        # time, and none of them is in a check's runtime closure. attic skips
        # paths signed by cache.nixos.org and paths the cache already has.
        printf '%s\n' "''${drvs[@]}" \
          | xargs -r nix-store --query --requisites --include-outputs \
          | grep -v '\.drv$' \
          | XDG_CONFIG_HOME=${atticConfigHome} attic push homelab:main --stdin
      '';
    })
  ];
}
