# An aarch64-linux NixOS VM that builds for the Mac's nix-daemon, so cache-push
# (hosts/common/attic.nix) can also push luneta's aarch64-linux check.
#
# The stock builder (1 core, 3 GiB) came first, since cache.nixos.org holds it
# and a configured one has to be built on aarch64-linux. This one is built by
# that one. 8 of the Mac's 12 cores and 10 of its 24 GiB: luneta's check is a
# Rust build plus ~1,450 tests. Two jobs at once, so two rustc-heavy builds fit
# in the memory. The disk stays at 20 GiB: the qcow2 image is made once, so a
# new diskSize would only apply after deleting /var/lib/linux-builder/nixos.qcow2.
{
  nix.linux-builder = {
    enable = true;
    maxJobs = 2;
    config = {
      virtualisation = {
        cores = 8;
        darwin-builder.memorySize = 10 * 1024;
      };
    };
  };
}
