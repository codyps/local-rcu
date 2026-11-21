{
  inputs = {
    nixpkgs.url = "nixpkgs/nixpkgs-unstable";
    # nixpkgs-unstable (26.11+) dropped x86_64-darwin, so Intel Macs must stay
    # on the last branch that still supports them.
    nixpkgs-darwin-x86_64.url = "github:NixOS/nixpkgs/nixpkgs-26.05-darwin";
    flake-utils.url = "github:numtide/flake-utils";
    crane.url = "github:ipetkov/crane";
  };

  outputs = { self, flake-utils, crane, nixpkgs, nixpkgs-darwin-x86_64 }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        # x86_64-darwin: silence the 26.05 "last release to support" warning;
        # staying pinned here is a deliberate choice, not an accident.
        pkgs =
          if system == "x86_64-darwin"
          then import nixpkgs-darwin-x86_64 {
            inherit system;
            config.allowDeprecatedx86_64Darwin = true;
          }
          else import nixpkgs { inherit system; };
        lib = pkgs.lib;
        stdenv = pkgs.stdenv;

        craneLib = (crane.mkLib pkgs);

        #src = craneLib.cleanCargoSource (craneLib.path ./.);
        src = let
          # Only keeps stderr (try-build) files
          stderrFilter = path: _type: builtins.match ".*\\.stderr$" path != null;
          stderrOrCargo = path: type:
            (stderrFilter path type) || (craneLib.filterCargoSources path type);
        in
          lib.cleanSourceWith {
            src = craneLib.path ./.;
            filter = stderrOrCargo;
          };
      in
      {
        packages.default = craneLib.buildPackage {
          inherit src;
          doCheck = true;
        };

        packages.default-ci = craneLib.buildPackage {
          inherit src;
          doCheck = true;
        };

        formatter = pkgs.nixpkgs-fmt;

        devShells.fmt = pkgs.mkShellNoCC {
          nativeBuildInputs = with pkgs; [cargo rustfmt];
        };

        devShells.clippy = pkgs.mkShell {
          nativeBuildInputs = with pkgs; [
            rustc
            cargo
            clippy
          ];
        };

        devShells.default = pkgs.mkShell {
          nativeBuildInputs = with pkgs; [
            rustc
            cargo
            rustfmt
            clippy
            rust-analyzer
          ];

          RUST_SRC_PATH = "${pkgs.rust.packages.stable.rustPlatform.rustLibSrc}";
        };
      }
    );
}
