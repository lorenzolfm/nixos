{
  config,
  lib,
  pkgs,
  ...
}:

let
  settingsFormat = pkgs.formats.yaml { };

  homelab2Host = "10.0.1.2";
  homelab2Port = 2222;
  hotRepoPathHomelab2 = "/data/repo/hot";

  # The contabo hostname and the uptime-kuma push URLs live in secrets.yaml,
  # not in local-secrets/: flake evaluation is pure (nixos-rebuild and CI
  # alike), and under pure eval `builtins.pathExists` on an out-of-tree path
  # returns false even when the file exists, so a pathExists fallback would
  # silently deploy a broken config. Instead the borgmatic YAML references
  # sops placeholders and is rendered with the real values at activation.
  contaboHost = config.sops.placeholder."contabo-host";
  contaboPort = 22;
  hotRepoPathContabo = "/srv/borg/lorenzo-desktop/hot";

  hotSourceDirectories = [
    "/home/lorenzo/.ssh"
    "/home/lorenzo/.gnupg"
    "/home/lorenzo/.sparrow"
    "/home/lorenzo/.claude"
    "/home/lorenzo/Documents"
    "/home/lorenzo/Desktop"
    "/home/lorenzo/Downloads"
  ];

  passphraseCommand = "cat ${config.sops.secrets."borg-passphrase".path}";
  sshCommandFor =
    destination:
    "ssh -o StrictHostKeyChecking=accept-new -i ${
      config.sops.secrets."borg-ssh-key-${destination}".path
    }";

  uptimeKumaHookFor = destination: {
    uptime_kuma.push_url = config.sops.placeholder."uptime-kuma-push-${destination}";
  };

  mkConfig =
    {
      sourceDirectories,
      repoPath,
      host,
      port ? 22,
      destination,
      retention ? {
        keep_daily = 7;
        keep_weekly = 4;
        keep_monthly = 6;
      },
      compression ? "auto,zstd",
      skipActions ? [ ],
      checks ? [
        {
          name = "repository";
          frequency = "2 weeks";
        }
        {
          name = "archives";
          frequency = "1 month";
        }
      ],
    }:
    {
      source_directories = sourceDirectories;
      repositories = [
        {
          path = "ssh://borg@${host}:${toString port}${repoPath}";
          label = "${baseNameOf repoPath}-${destination}";
        }
      ];
      encryption_passcommand = passphraseCommand;
      ssh_command = sshCommandFor destination;
      compression = compression;
      inherit checks;
    }
    // retention
    // uptimeKumaHookFor destination
    // (if skipActions == [ ] then { } else { skip_actions = skipActions; });
in
{
  sops.age.keyFile = "/var/lib/sops-nix/key.txt";
  sops.age.generateKey = true;
  sops.age.sshKeyPaths = [ ];

  sops.defaultSopsFile = ./secrets.yaml;
  sops.secrets."borg-passphrase" = { };
  sops.secrets."borg-ssh-key-homelab2" = {
    mode = "0400";
  };
  sops.secrets."borg-ssh-key-contabo" = {
    mode = "0400";
  };
  sops.secrets."contabo-host" = { };
  sops.secrets."uptime-kuma-push-homelab2" = { };
  sops.secrets."uptime-kuma-push-contabo" = { };

  # Render every borgmatic config through sops so the placeholders above are
  # substituted at activation, then point /etc/borgmatic.d/<name>.yaml at the
  # rendered file instead of the placeholder-bearing store copy. The module's
  # build-time `borgmatic config validate` still runs on the store copy.
  sops.templates = lib.mapAttrs' (
    name: cfg:
    lib.nameValuePair "borgmatic-${name}.yaml" {
      file = settingsFormat.generate "${name}.yaml" cfg;
    }
  ) config.services.borgmatic.configurations;
  environment.etc = lib.mapAttrs' (
    name: _:
    lib.nameValuePair "borgmatic.d/${name}.yaml" {
      source = lib.mkForce config.sops.templates."borgmatic-${name}.yaml".path;
    }
  ) config.services.borgmatic.configurations;

  services.borgmatic = {
    enable = true;
    configurations = {
      hot-homelab2 = mkConfig {
        sourceDirectories = hotSourceDirectories;
        repoPath = hotRepoPathHomelab2;
        host = homelab2Host;
        port = homelab2Port;
        destination = "homelab2";
      };
      hot-contabo = mkConfig {
        sourceDirectories = hotSourceDirectories;
        repoPath = hotRepoPathContabo;
        host = contaboHost;
        port = contaboPort;
        destination = "contabo";
        skipActions = [
          "compact"
          "check"
        ];
      };
    };
  };

  systemd.timers.borgmatic.enable = true;
}
