{ pkgs, ... }:

{
  # Also turns on 32-bit graphics, which pulls in the NVIDIA driver's lib32
  # half that Proton and most Windows games need, and the udev rules that
  # let Steam talk to controllers without root.
  programs.steam = {
    enable = true;
    # GE-Proton ships per-game protonfixes. Assetto Corsa's launcher crashes
    # under wine-mono (WineHQ bug 58033) and only runs on Microsoft's .NET
    # 4.8, which GE-Proton's fix for app 244210 installs on first launch.
    # Pick it per game under Properties > Compatibility.
    extraCompatPackages = [ pkgs.proton-ge-bin ];
  };

  # The in-kernel driver gives the G29 constant force only, so Assetto
  # Corsa's damper effect (the stiff wheel at standstill) silently does
  # nothing. new-lg4ff adds damper, spring and friction. Both modules claim
  # the wheel under the same HID driver name, so the stock one is
  # blacklisted or it can win the race at boot. The wheel must still sit
  # in PS3 mode.
  hardware.new-lg4ff.enable = true;
  boot.blacklistedKernelModules = [ "hid_logitech" ];

  # DuckStation (PS1) is missing on purpose: nixpkgs dropped it at upstream's
  # request, so it runs from the upstream AppImage in ~/Applications through
  # appimage-run, and updates itself from there.
  environment.systemPackages = [ pkgs.pcsx2 ];
}
