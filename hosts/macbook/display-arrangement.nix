{ pkgs, ... }:

# Keeps the desk's displays in their physical order, Samsung, ASUS, MacBook, with
# the ASUS as the main display. nix-darwin has no option for the arrangement,
# and macOS loses it when the lid opens, so a login agent reapplies it whenever
# the displays change. See display-arrangement.swift.
let
  display-arrangement =
    pkgs.runCommand "display-arrangement"
      {
        nativeBuildInputs = [ pkgs.swift ];
      }
      ''
        mkdir -p $out/bin
        swiftc -O ${./display-arrangement.swift} -o $out/bin/display-arrangement
      '';
in
{
  environment.systemPackages = [ display-arrangement ];

  launchd.user.agents.display-arrangement.serviceConfig = {
    ProgramArguments = [ "${display-arrangement}/bin/display-arrangement" ];
    RunAtLoad = true;
    KeepAlive = true;
    # It needs the window server, which only a GUI login session has.
    LimitLoadToSessionType = "Aqua";
    ProcessType = "Interactive";
  };
}
