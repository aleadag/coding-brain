{
  description = "Local brain for supervising and learning from coding-agent activity.";

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
          pname = "coding-brain";
          version = cargoToml.package.version;
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;
          nativeCheckInputs = [ pkgs.git ];

          meta = with pkgs.lib; {
            description = "Local brain for supervising and learning from coding-agent activity.";
            homepage = "https://github.com/aleadag/coding-brain";
            license = licenses.mit;
            mainProgram = "coding-brain";
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
          env.GH_REPO = "aleadag/coding-brain";
        };
      }
    )
    // {
      homeManagerModules.default = homeManagerModule;
      homeModules.default = homeManagerModule;
    };
}
