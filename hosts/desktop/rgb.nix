{ pkgs, ... }:

{
  # Motherboard (MSI Mystic Light), RAM, and the NZXT / Cooler Master fan
  # controllers all expose their LEDs to OpenRGB. `motherboard` is set
  # explicitly because it otherwise defaults off hardware.cpu.*.updateMicrocode,
  # which hardware-configuration.nix still reports as intel on this Ryzen box --
  # that would load i2c-i801 instead of the i2c-piix4 this AMD board needs, and
  # the DIMMs would never be found.
  services.hardware.openrgb = {
    enable = true;
    motherboard = "amd";
    # Applied at boot by the server, which reads profiles from its own
    # StateDirectory -- a profile saved by a client lands in ~/.config/OpenRGB
    # instead and has to be copied to /var/lib/OpenRGB to be picked up here.
    startupProfile = "gruvbox";
  };

  # spd5118 binds the DDR5 SPD hubs and holds the SMBus addresses OpenRGB needs
  # to reach the DIMMs' RGB controllers (i2cdetect shows them as UU while it is
  # loaded). Costs the per-DIMM temperature sensors, buys back RAM lighting.
  boot.blacklistedKernelModules = [ "spd5118" ];

  # i2cdetect, for checking whether the SMBus addresses are actually free.
  environment.systemPackages = [ pkgs.i2c-tools ];
}
