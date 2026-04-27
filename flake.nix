{
  description = "My NixOS enjoyrr config";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    darwin.url = "github:LnL7/nix-darwin/master";
    darwin.inputs.nixpkgs.follows = "nixpkgs";
    mac-app-util.url = "github:hraban/mac-app-util";
    nix-homebrew.url = "github:zhaofengli-wip/nix-homebrew/main";
    claude-code.url = "github:sadjow/claude-code-nix";
  };

  outputs =
    {
      self,
      nixpkgs,
      darwin,
      mac-app-util,
      nix-homebrew,
      claude-code,
      ...
    }@inputs:
    {
      nixosConfigurations.nixos = nixpkgs.lib.nixosSystem {
        system = "x86_64-linux";
        specialArgs = { inherit claude-code; };
        modules = [
          ./hosts/desktop/configuration.nix
        ];
      };

      darwinConfigurations.macbook = darwin.lib.darwinSystem {
        system = "aarch64-darwin";
        specialArgs = { inherit claude-code; };
        modules = [
          {
            nixpkgs.overlays = [
              (_final: prev: {
                direnv = prev.direnv.overrideAttrs { doCheck = false; };
              })
            ];
          }
          mac-app-util.darwinModules.default
          nix-homebrew.darwinModules.nix-homebrew
          {
            nix-homebrew = {
              enable = true;
              enableRosetta = true;
              user = "lorenzo";
            };
          }
          ./hosts/macbook/configuration.nix
        ];
      };
    };
}
