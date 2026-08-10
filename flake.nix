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
        package = pkgs.rustPlatform.buildRustPackage {
          pname = "coding-brain";
          version = cargoToml.package.version;
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;
          checkType = "debug";
          dontUseCargoParallelTests = true;
          cargoTestFlags = [
            "-p"
            "coding-brain-core"
            "-p"
            "coding-brain-tui"
          ]
          ++ pkgs.lib.optionals pkgs.stdenv.isDarwin [
            "--"
            "--skip"
            "helpers::tests::status_webhook_keeps_only_retained_session_fields"
            "--skip"
            "project::tests::git_root_preserves_non_utf8_path_bytes"
          ];
          postCheck = ''
            cargo test \
              --target ${pkgs.stdenv.hostPlatform.rust.rustcTarget} \
              --offline \
              --test release_workflow \
              --test release_metadata \
              -- \
              --test-threads=1
          '';
          nativeCheckInputs = [
            pkgs.git
            pkgs.curl
          ];

          meta = with pkgs.lib; {
            description = "Local brain for supervising and learning from coding-agent activity.";
            homepage = "https://github.com/aleadag/coding-brain";
            license = licenses.mit;
            mainProgram = "cbrain";
            platforms = platforms.unix;
          };
        };
        doctorFixtures = import ./nix/tests/home-manager-doctor-fixtures.nix {
          inherit
            home-manager
            package
            pkgs
            self
            ;
        };
        storageSecurityVm = pkgs.testers.runNixOSTest (
          import ./nix/tests/storage-security-vm.nix {
            inherit doctorFixtures package;
          }
        );
      in
      {
        packages.default = package;

        checks = {
          home-manager-module = import ./nix/tests/home-manager-module.nix {
            inherit home-manager pkgs self;
          };
        }
        // pkgs.lib.optionalAttrs (system == "x86_64-linux") {
          storage-security-vm = storageSecurityVm;
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
