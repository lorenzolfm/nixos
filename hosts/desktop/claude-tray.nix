{ lib, ... }:

# Autostart for the Claude Code session tray applet (~/Projects/misc/claude-tray).
#
# The applet publishes a StatusNotifierItem so the state of many concurrent Claude Code sessions
# is ambient in Waybar. Waybar needs no configuration change: its `tray` module is already in
# `modules-right`, and an SNI item joins it simply by existing on the session bus.
#
# Why a user service and not a Hyprland `exec-once`: the failure this whole effort exists to
# prevent is a session indicator that quietly disappears. `exec-once` gives no restart and no
# journal; a unit gives both. Read it with `journalctl --user -u claude-tray`.

let
  home = "/home/lorenzo";
in
{
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
    # the manager's own PATH from /etc/environment.d does NOT reach the service. Both binaries
    # the applet shells out to would be missing. Neither is pinned to a store path on purpose:
    #   - claude-agents, so the producer can be upgraded underneath the applet;
    #   - zellij, because click-to-jump speaks to a running server and a pinned build of a
    #     different version would talk to it wrongly. It must be the same zellij he runs.
    environment.PATH = lib.mkForce (
      lib.concatStringsSep ":" [
        "${home}/.nix-profile/bin"
        "/etc/profiles/per-user/lorenzo/bin"
        "/run/current-system/sw/bin"
      ]
    );

    serviceConfig = {
      # Installed with `nix profile add .` from the claude-tray checkout, the same way
      # `claude-agents` itself is installed.
      ExecStart = "${home}/.nix-profile/bin/claude-tray";
      Restart = "always";
      RestartSec = 5;
    };

    unitConfig = {
      # Every user with a systemd manager gets this unit — including gdm-greeter at the login
      # screen. Only Lorenzo has the applet or a bar to put it in.
      ConditionUser = "lorenzo";
      ConditionPathExists = "${home}/.nix-profile/bin/claude-tray";

      # Never give up. A rate limit here would turn a bad minute into a permanently empty bar,
      # which is the exact failure this unit exists to rule out.
      StartLimitIntervalSec = 0;
    };
  };
}
