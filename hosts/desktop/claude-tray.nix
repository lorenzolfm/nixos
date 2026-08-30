{
  lib,
  pkgs,
  claude-tray,
  ...
}:

# The Claude Code session tray applet (github.com/lorenzolfm/claude-tray): which sessions are
# waiting on you, as a glyph and a count in Waybar, with the list one click away.
#
# Waybar needs no configuration change — its `tray` module is already in `modules-right`, and an
# SNI item joins it simply by existing on the session bus.
#
# Why a user service and not a Hyprland `exec-once`: the failure this applet exists to prevent is
# a session indicator that quietly disappears. `exec-once` gives no restart and no journal; a unit
# gives both. Read it with `journalctl --user -u claude-tray`.

let
  package = claude-tray.packages.${pkgs.stdenv.hostPlatform.system}.default;
in
{
  # Also on PATH, so it can be run and debugged by hand.
  environment.systemPackages = [ package ];

  systemd.user.services.claude-tray = {
    description = "Claude Code session tray applet";
    documentation = [ "https://github.com/lorenzolfm/claude-tray" ];

    # The session bus is the only real prerequisite. This deliberately does NOT order itself
    # after Waybar: Waybar is a Hyprland `exec-once` and therefore always starts later than the
    # user manager, so waiting for it is impossible by construction. The applet instead calls
    # ksni's `assume_sni_available`, which turns "no tray host yet" into a wait and re-registers
    # the item whenever a host appears — at first login and after every Waybar restart alike.
    requires = [ "dbus.socket" ];
    after = [ "dbus.socket" ];

    # `graphical-session.target` would be the tidier binding, but it only activates under the
    # UWSM-managed session; a plain `hyprland` login leaves it dead, which would silently mean
    # no tray at all. The applet needs no compositor anyway — it is a D-Bus service.
    wantedBy = [ "default.target" ];

    # 🔴 NixOS gives user units a sanitized PATH (coreutils, findutils, grep, sed, systemd), so
    # the manager's own PATH from /etc/environment.d does NOT reach the service, and both
    # binaries the applet shells out to would be missing.
    #
    # `claude-ps` is a system package now (`./claude-ps.nix`), so it arrives on
    # /run/current-system/sw/bin with everything else and moves in the same generation as the
    # applet — which is what its contract wants, the two agreeing on key names. It used to be an
    # imperative `nix profile` install, which is why ~/.nix-profile/bin was on this list; that
    # entry is gone, and nothing the applet calls lives there any more.
    #
    # `zellij` is still looked up by name rather than by store path, because click-to-jump speaks
    # to a **running** server and a pinned build of a different version would talk to it wrongly.
    # It must be the same zellij he runs.
    environment.PATH = lib.mkForce (
      lib.concatStringsSep ":" [
        "/etc/profiles/per-user/lorenzo/bin"
        "/run/current-system/sw/bin"
      ]
    );

    serviceConfig = {
      ExecStart = lib.getExe package;
      Restart = "always";
      RestartSec = 5;
    };

    unitConfig = {
      # Every user with a systemd manager gets this unit — including gdm-greeter at the login
      # screen. Only Lorenzo has a bar to put it in.
      ConditionUser = "lorenzo";

      # Never give up. A rate limit here would turn a bad minute into a permanently empty bar,
      # which is the exact failure this unit exists to rule out.
      StartLimitIntervalSec = 0;
    };
  };
}
