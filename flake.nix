{
  inputs = {
    nixpkgs.url = "nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    naersk = {
      url = "github:nix-community/naersk";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, flake-utils, naersk, nixpkgs }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = ((import nixpkgs) {
          inherit system;
        });

        naersk' = pkgs.callPackage naersk { };
      in
      {
        packages.default = naersk'.buildPackage {
          src = ./.;
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
          ];

          RUSTC_WRAPPER = "sccache";
          RUST_SRC_PATH = "${pkgs.rust.packages.stable.rustPlatform.rustLibSrc}";
        };
      }
    );
}
