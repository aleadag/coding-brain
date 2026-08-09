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
        nixCheckEntrypoint =
          if pkgs.stdenv.isLinux then
            pkgs.writeShellScript "coding-brain-nix-check" ''
              ${pkgs.coreutils}/bin/chmod 0755 /
              ${pkgs.coreutils}/bin/chmod 1777 /tmp
              exec "$CBRAIN_NIX_REAL_CARGO" "$@"
            ''
          else
            null;
        nixCheckCargo =
          if pkgs.stdenv.isLinux then
            pkgs.writeShellScriptBin "cargo" ''
              exec ${pkgs.bubblewrap}/bin/bwrap \
                --die-with-parent \
                --unshare-user \
                --uid "$CBRAIN_NIX_CHECK_UID" \
                --gid "$CBRAIN_NIX_CHECK_GID" \
                --tmpfs / \
                --dir /nix \
                --ro-bind /nix/store /nix/store \
                --dir "$NIX_BUILD_TOP" \
                --bind "$NIX_BUILD_TOP" "$NIX_BUILD_TOP" \
                --dir /bin \
                --ro-bind /bin /bin \
                --proc /proc \
                --dev /dev \
                --dir /tmp \
                --tmpfs /tmp \
                --chdir "$PWD" \
                ${nixCheckEntrypoint} "$@"
            ''
          else
            null;
      in
      {
        packages.default = pkgs.rustPlatform.buildRustPackage {
          pname = "coding-brain";
          version = cargoToml.package.version;
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;
          checkType = "debug";
          dontUseCargoParallelTests = true;
          preCheck = pkgs.lib.optionalString pkgs.stdenv.isLinux ''
            export CBRAIN_NIX_REAL_CARGO="$(command -v cargo)"
            export CBRAIN_NIX_CHECK_UID="$(${pkgs.coreutils}/bin/id -u)"
            export CBRAIN_NIX_CHECK_GID="$(${pkgs.coreutils}/bin/id -g)"
            export PATH="${nixCheckCargo}/bin:$PATH"
          '';
          nativeCheckInputs = [ pkgs.git ];

          meta = with pkgs.lib; {
            description = "Local brain for supervising and learning from coding-agent activity.";
            homepage = "https://github.com/aleadag/coding-brain";
            license = licenses.mit;
            mainProgram = "cbrain";
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
