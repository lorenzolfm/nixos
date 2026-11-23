# The Mac's secrets, decrypted at activation with an age key made from its SSH
# host key (recipient `macbook` in .sops.yaml), so there is no key file to keep.
{
  sops = {
    defaultSopsFile = ./secrets.yaml;
    age.sshKeyPaths = [ "/etc/ssh/ssh_host_ed25519_key" ];
  };
}
