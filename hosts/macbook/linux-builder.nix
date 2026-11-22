# An aarch64-linux NixOS VM that builds for the Mac's nix-daemon, so cache-push
# (hosts/common/attic.nix) can also push luneta's aarch64-linux check.
#
# This is the stock builder, which cache.nixos.org already holds: 1 core, 3 GiB
# and 20 GiB of disk. Building a configured one (more cores and memory) needs
# an aarch64-linux builder, so that comes in a second switch, once this one is
# up.
{
  nix.linux-builder.enable = true;
}
