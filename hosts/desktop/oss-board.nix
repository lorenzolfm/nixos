# Floresta triage: refreshes the board hourly, DMs twice a day.
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

  # systemd services get a minimal PATH -- /run/current-system/sw/bin is NOT on
  # it. Every binary the run shells out to must be listed here explicitly, or
  # the model passes fail open and the board silently degrades to ranking order.
  runtimeDeps = [
    claude-code.packages.${pkgs.system}.claude-code
  ]
  ++ (with pkgs; [
    python3
    gh
    # git reads your local clone for unshipped branches and the footprint.
    git
    coreutils
    openssh
    cacert
  ]);
in
{
  # Read-only GitHub token: a CLASSIC PAT whose only scope is `read:project`.
  # With no `repo` or `public_repo` scope it can read public repositories and
  # nothing private anywhere -- so it cannot comment, claim, or open a PR as
  # lorenzolfm, and cannot see an employer repo at all. `read:project` is what
  # the org board's Needs Review column needs; fine-grained tokens would need
  # an org admin's approval for that.
  sops.secrets."oss-board-github-token" = {
    owner = user;
    mode = "0400";
  };
  sops.secrets."oss-board-telegram-token" = {
    owner = user;
    mode = "0400";
  };
  # Read at run time from the sops file, not at eval time from local-secrets/:
  # flake eval is pure and `builtins.pathExists` on an out-of-tree path is
  # false there, which silently dropped the heartbeat on every plain rebuild.
  sops.secrets."uptime-kuma-push-oss-board" = {
    owner = user;
    mode = "0400";
  };

  systemd.services.oss-board = {
    description = "Refresh the Floresta triage board";
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
      SSL_CERT_FILE = "${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";
      TELEGRAM_CHAT_ID = telegramChatId;
      TELEGRAM_BOT_TOKEN_FILE = config.sops.secrets."oss-board-telegram-token".path;
      # openclaw's /api/channels/telegram exists (answers 401, not 404) but its
      # request body shape is unconfirmed. Flip to "1" once verified.
      OPENCLAW_ENABLED = "0";
      OPENCLAW_URL = "https://oc.lorenzo.sh";
    };

    serviceConfig = {
      Type = "oneshot";
      User = user;
      Group = "users";
      WorkingDirectory = projectDir;

      # GH_TOKEN and the push URL must come from files, not the unit, so they
      # never land in the journal or in /proc/*/environ.
      ExecStart = pkgs.writeShellScript "oss-board-run" ''
        set -euo pipefail
        export GH_TOKEN="$(cat ${config.sops.secrets."oss-board-github-token".path})"
        export UPTIME_KUMA_PUSH_URL="$(cat ${config.sops.secrets."uptime-kuma-push-oss-board".path})"
        exec ${pkgs.python3}/bin/python3 ${projectDir}/run.py \
          --board ${lib.escapeShellArg board} \
          --notify
      '';

      # A snapshot plus the model passes takes a few minutes; give up well
      # before the next hourly firing rather than overlap it.
      TimeoutStartSec = "20min";

      Nice = 10;
      IOSchedulingClass = "idle";

      # Hardening. It writes the vault and its own state dir and nothing else;
      # the Floresta clone in ~/Projects/misc is only read.
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
    description = "Refresh the Floresta triage board hourly";
    wantedBy = [ "timers.target" ];
    timerConfig = {
      # Hourly for the board. The DM is not a timer concern: run.py sends it on
      # the first run at or after each slot in config.DM_SLOTS (08:07, 19:23),
      # which is why 19:23 is listed too. Off-peak minutes on purpose: :00 and
      # :30 are where every scheduler lands.
      OnCalendar = [
        "*-*-* *:07:00"
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
