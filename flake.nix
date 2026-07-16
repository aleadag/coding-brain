{
  description = "Orchestrate Codex sessions with a local-LLM brain that learns from you.";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    home-manager = {
      url = "github:nix-community/home-manager";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
      home-manager,
    }:
    let
      homeManagerModule = import ./nix/home-manager.nix { inherit self; };
    in
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
        cargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);
      in
      {
        packages.default = pkgs.rustPlatform.buildRustPackage {
          pname = "codexctl";
          version = cargoToml.package.version;
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;
          nativeCheckInputs = [ pkgs.git ];

          meta = with pkgs.lib; {
            description = "Orchestrate Codex sessions with a local-LLM brain that learns from you.";
            homepage = "https://github.com/aleadag/codexctl";
            license = licenses.mit;
            mainProgram = "codexctl";
            platforms = platforms.unix;
          };
        };

        checks.home-manager-module = import ./nix/tests/home-manager-module.nix {
          inherit home-manager pkgs self;
        };

        formatter = pkgs.nixfmt-rfc-style;

        devShells.default = pkgs.mkShell {
          buildInputs = with pkgs; [
            rustc
            cargo
            clippy
            rustfmt
            sqlite
          ];
          env.GH_REPO = "aleadag/codexctl";
        };
      }
    )
    // {
      homeManagerModules.default = homeManagerModule;
      homeModules.default = homeManagerModule;
    };
}
