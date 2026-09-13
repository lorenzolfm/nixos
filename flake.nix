{
  description = "My NixOS enjoyrr config";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

    # Third-party tooling
    claude-code = {
      url = "github:sadjow/claude-code-nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    sops-nix = {
      url = "github:Mic92/sops-nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    # macOS only
    darwin = {
      url = "github:LnL7/nix-darwin/master";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    mac-app-util.url = "github:hraban/mac-app-util";
    nix-homebrew.url = "github:zhaofengli-wip/nix-homebrew/main";

    # Mine
    wt = {
      url = "github:lorenzolfm/wt";
      inputs.nixpkgs.follows = "nixpkgs";
      inputs.rust-overlay.follows = "rust-overlay";
    };
    claude-tray = {
      url = "github:lorenzolfm/claude-tray";
      inputs.nixpkgs.follows = "nixpkgs";
      inputs.rust-overlay.follows = "rust-overlay";
    };
    claude-ps = {
      url = "github:lorenzolfm/claude-ps";
      inputs.nixpkgs.follows = "nixpkgs";
      inputs.rust-overlay.follows = "rust-overlay";
    };
    claude-nav = {
      url = "github:lorenzolfm/claude-nav";
      inputs.nixpkgs.follows = "nixpkgs";
      inputs.rust-overlay.follows = "rust-overlay";
    };
  };

  outputs =
    {
      nixpkgs,
      darwin,
      mac-app-util,
      nix-homebrew,
      claude-code,
      rust-overlay,
      sops-nix,
      wt,
      claude-tray,
      claude-ps,
      claude-nav,
      ...
    }:
    {
      nixosConfigurations.nixos = nixpkgs.lib.nixosSystem {
        system = "x86_64-linux";
        specialArgs = {
          inherit
            claude-code
            rust-overlay
            wt
            claude-tray
            claude-ps
            claude-nav
            ;
        };
        modules = [
          ./hosts/desktop/configuration.nix
          sops-nix.nixosModules.sops
        ];
      };

      darwinConfigurations.macbook = darwin.lib.darwinSystem {
        system = "aarch64-darwin";
        specialArgs = {
          inherit
            claude-code
            rust-overlay
            wt
            claude-ps
            ;
        };
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
