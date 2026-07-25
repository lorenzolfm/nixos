{ ... }:

{
  services.borgbackup.repos.lnd-scb = {
    path = "/var/lib/borg/lnd-scb";
    authorizedKeys = [
      "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIMcsARNbBOzArvVwnonv/GRF6T7g7ZauguT57gS3q2h0 borg-lnd-scb-to-desktop"
    ];
    quota = "1G";
    allowSubRepos = false;
  };
  networking.firewall.extraInputRules = ''
    iifname "enp7s0" ip saddr 10.0.1.4 tcp dport 22 accept comment "borg-lnd-scb: homelab-2 -> onsite SCB repo"
  '';
}
