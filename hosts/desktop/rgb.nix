{ pkgs, ... }:

{
  services.hardware.openrgb = {
    enable = true;
    motherboard = "amd";
    startupProfile = "gruvbox";
  };

  boot.blacklistedKernelModules = [ "spd5118" ];
  environment.systemPackages = [ pkgs.i2c-tools ];
}
