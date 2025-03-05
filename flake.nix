{
  description = "My NixOS enjoyrr config";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    darwin.url = "github:LnL7/nix-darwin/master";
    darwin.inputs.nixpkgs.follows = "nixpkgs";
    mac-app-util.url = "github:hraban/mac-app-util";
  };

  outputs =
    {
      self,
      nixpkgs,
      darwin,
      mac-app-util,
      ...
    }@inputs:
    {
      nixosConfigurations.nixos = nixpkgs.lib.nixosSystem {
        system = "x86_64-linux";
        modules = [
          ./hosts/desktop/configuration.nix
        ];
      };

      darwinConfigurations.macbook = darwin.lib.darwinSystem {
        system = "aarch64-darwin";
        modules = [
          mac-app-util.darwinModules.default
          ./hosts/macbook/configuration.nix
        ];
      };
    };
}
