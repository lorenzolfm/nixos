# Twice-daily open-source contribution board updater.
#
# Mirrors the borgmatic pattern in backup.nix: a systemd timer with
# Persistent = true, secrets via sops-nix, and an uptime-kuma push heartbeat so
# a job that silently stops updating the board is visible.
#
# Import from hosts/desktop/configuration.nix:
#   imports = [ ./oss-board.nix ];
# `claude-code` arrives via specialArgs in flake.nix, the same way
# claude-tray.nix and claude-ps.nix receive theirs.
{
  config,
  pkgs,
  lib,
  claude-code,
  ...
}:
let
  user = "lorenzo";
  home = "/home/${user}";
  projectDir = "${home}/Projects/misc/opensource";
  board = "${home}/Documents/Vault/Opensource/Tasks.md";

  # Telegram chat id is the openclaw allowlist entry -- not a secret.
  telegramChatId = "507707481";

  pushUrlFile = "${home}/.config/nixos/local-secrets/uptime-kuma-push-oss-board";
  hasPushUrl = builtins.pathExists pushUrlFile;
  pushUrl =
    if hasPushUrl then builtins.replaceStrings [ "\n" ] [ "" ] (builtins.readFile pushUrlFile) else "";

  # systemd services get a minimal PATH -- /run/current-system/sw/bin is NOT on
  # it. Every binary the run shells out to must be listed here explicitly, or
  # the model passes fail open and the board silently degrades to ranking order.
  runtimeDeps = [
    claude-code.packages.${pkgs.system}.claude-code
  ]
  ++ (with pkgs; [
    python3
    gh
    git
    nix
    coreutils
    openssh
    cacert
  ]);
in
{
  # Read-only GitHub token, minted as a fine-grained PAT with no write scopes.
  # The read-only guarantee is structural: even a misbehaving run cannot comment,
  # claim, or open a PR as lorenzolfm.
  sops.secrets."oss-board-github-token" = {
    owner = user;
    mode = "0400";
  };
  sops.secrets."oss-board-telegram-token" = {
    owner = user;
    mode = "0400";
  };

  systemd.services.oss-board = {
    description = "Update the open-source contribution board";
    after = [ "network-online.target" ];
    wants = [ "network-online.target" ];

    # The timer owns when this runs. Without this, a rebuild that changes the
    # unit stops an in-flight scan and restarts it -- and since it is oneshot,
    # `nixos-rebuild` then blocks on the whole scan (up to TimeoutStartSec).
    # A changed definition still lands in /etc and takes effect on next firing.
    restartIfChanged = false;

    path = runtimeDeps;

    environment = {
      HOME = home;
      # `nix eval` for the nixpkgs bump check, and TLS for the API calls.
      NIX_PATH = "nixpkgs=flake:nixpkgs";
      SSL_CERT_FILE = "${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";
      TELEGRAM_CHAT_ID = telegramChatId;
      TELEGRAM_BOT_TOKEN_FILE = config.sops.secrets."oss-board-telegram-token".path;
      # openclaw's /api/channels/telegram exists (answers 401, not 404) but its
      # request body shape is unconfirmed. Flip to "1" once verified.
      OPENCLAW_ENABLED = "0";
      OPENCLAW_URL = "https://oc.lorenzo.sh";
    }
    // lib.optionalAttrs hasPushUrl { UPTIME_KUMA_PUSH_URL = pushUrl; };

    serviceConfig = {
      Type = "oneshot";
      User = user;
      Group = "users";
      WorkingDirectory = projectDir;

      # GH_TOKEN must come from the file, not the unit, so it never lands in the
      # journal or in /proc/*/environ.
      ExecStart = pkgs.writeShellScript "oss-board-run" ''
        set -euo pipefail
        export GH_TOKEN="$(cat ${config.sops.secrets."oss-board-github-token".path})"
        exec ${pkgs.python3}/bin/python3 ${projectDir}/run.py \
          --board ${lib.escapeShellArg board} \
          --notify
      '';

      # A scan plus the model passes; generous ceiling, then give up rather than
      # overlap with the next timer firing.
      TimeoutStartSec = "45min";

      Nice = 10;
      IOSchedulingClass = "idle";

      # Hardening. It needs the vault, the project dir, and ~/Projects/misc for
      # the stranded-branch scan -- and nothing else writable.
      PrivateTmp = true;
      ProtectSystem = "strict";
      ProtectHome = "read-only";
      ReadWritePaths = [
        "${home}/Documents/Vault/Opensource"
        "${projectDir}/state"
      ];
      NoNewPrivileges = true;
      ProtectKernelTunables = true;
      ProtectKernelModules = true;
      ProtectControlGroups = true;
      RestrictSUIDSGID = true;
      RestrictNamespaces = true;
      LockPersonality = true;
      MemoryDenyWriteExecute = false; # the model CLI needs a JIT-capable heap
      SystemCallArchitectures = "native";
    };
  };

  systemd.timers.oss-board = {
    description = "Update the open-source contribution board twice daily";
    wantedBy = [ "timers.target" ];
    timerConfig = {
      # Off-peak minutes on purpose: :00 and :30 are where every scheduler lands.
      OnCalendar = [
        "08:07"
        "19:23"
      ];
      # A run missed while the desktop was off fires on next boot rather than
      # being skipped -- the desktop is not always awake.
      Persistent = true;
      RandomizedDelaySec = "5m";
      Unit = "oss-board.service";
    };
  };
}
