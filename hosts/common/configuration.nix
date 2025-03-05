{ config, pkgs, ... }:

{
  environment.systemPackages = with pkgs; [
    neofetch
  ];
}
