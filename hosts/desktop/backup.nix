{ config, pkgs, ... }:

let
  homelab2Host = "10.0.1.2";
  homelab2Port = 2222;
  hotRepoPathHomelab2 = "/data/repo/hot";

  contaboHost = builtins.replaceStrings [ "\n" ] [ "" ] (
    builtins.readFile "/home/lorenzo/.config/nixos/local-secrets/contabo-host"
  );
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

  uptimeKumaHookFor =
    destination:
    let
      pushUrlFile = "/home/lorenzo/.config/nixos/local-secrets/uptime-kuma-push-${destination}";
    in
    if builtins.pathExists pushUrlFile then
      {
        uptime_kuma.push_url = builtins.replaceStrings [ "\n" ] [ "" ] (builtins.readFile pushUrlFile);
      }
    else
      { };

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
