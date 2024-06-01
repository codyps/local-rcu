{
  inputs = {
    nixpkgs.url = "nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    crane = {
      url = "github:ipetkov/crane";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, flake-utils, crane, nixpkgs }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = ((import nixpkgs) {
          inherit system;
        });
        lib = pkgs.lib;
        stdenv = pkgs.stdenv;

        craneLib = (crane.mkLib nixpkgs.legacyPackages.${system});

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

          RUSTC_WRAPPER = "sccache";
          nativeBuildInputs = with pkgs; [
            sccache
          ];
        };

        formatter = pkgs.nixpkgs-fmt;

        devShells.fmt = pkgs.mkShellNoCC {
          nativeBuildInputs = with pkgs; [cargo rustfmt];
        };

        devShells.clippy = pkgs.mkShell {
          nativeBuildInputs = with pkgs; [
            rustc
            cargo
            sccache
            clippy
          ];
        };

        devShells.default = pkgs.mkShell {
          nativeBuildInputs = with pkgs; [
            rustc
            cargo
            rustfmt
            sccache
            clippy
            rust-analyzer
          ] ++ lib.optional stdenv.isDarwin [
            iconv
          ];

          RUSTC_WRAPPER = "sccache";
          RUST_SRC_PATH = "${pkgs.rust.packages.stable.rustPlatform.rustLibSrc}";
        };
      }
    );
}
