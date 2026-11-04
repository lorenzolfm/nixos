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
    llm-agents = {
      url = "github:numtide/llm-agents.nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    # macOS only
    darwin = {
      url = "github:LnL7/nix-darwin/master";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    mac-app-util = {
      url = "github:hraban/mac-app-util";
      # Its own nixpkgs pins SBCL 2.6.4, which cannot map its heap at
      # 0x300100000 on macOS 27. Ours carries 2.6.8, where that is fixed.
      inputs.nixpkgs.follows = "nixpkgs";
    };
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
    luneta = {
      url = "github:lorenzolfm/luneta";
      inputs.nixpkgs.follows = "nixpkgs";
      inputs.rust-overlay.follows = "rust-overlay";
    };
    oss-board = {
      url = "github:lorenzolfm/oss-board";
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
      llm-agents,
      wt,
      claude-tray,
      claude-ps,
      claude-nav,
      luneta,
      oss-board,
      ...
    }:
    {
      nixosConfigurations.nixos = nixpkgs.lib.nixosSystem {
        system = "x86_64-linux";
        specialArgs = {
          inherit
            claude-code
            rust-overlay
            llm-agents
            wt
            claude-tray
            claude-ps
            claude-nav
            luneta
            oss-board
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
            llm-agents
            wt
            claude-ps
            luneta
            ;
        };
        modules = [
          {
            nixpkgs.overlays = [
              (_final: prev: {
                direnv = prev.direnv.overrideAttrs { doCheck = false; };
                # Nix builds unsandboxed on macOS, so these tests see every
                # process on the Mac. Pi's liveness check takes any process it
                # cannot read for a running Pi, and the tests fail.
                atuin = prev.atuin.overrideAttrs (old: {
                  checkFlags = (old.checkFlags or [ ]) ++ [
                    "--skip=a_diverged_copy_resumes_only_as_chosen"
                    "--skip=a_session_gone_on_with_elsewhere_catches_up_where_it_came_from"
                  ];
                });
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
