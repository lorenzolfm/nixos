{
  pkgs,
  claude-nav,
  ...
}:

# The vocabulary and the jump behind the Claude Code session surfaces
# (github.com/lorenzolfm/claude-nav): what a status means, what a row is called,
# what order the rows come in, and how the focus moves to an agent's zellij pane.
#
# claude-tray does not need this package. It links the crate, so its menu holds
# Rust values and no subprocess sits in its poll loop.
#
# The vicinae extension does. It has no Rust in it and reads `claude-nav list`
# and calls `claude-nav jump`, and it inherits the PATH of the vicinae server --
# which is a compositor child, not a systemd user unit, so a system package is
# the way it arrives.

{
  environment.systemPackages = [
    claude-nav.packages.${pkgs.stdenv.hostPlatform.system}.default
  ];
}
