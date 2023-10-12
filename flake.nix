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
        };

        formatter = pkgs.nixpkgs-fmt;

        devShell = pkgs.mkShell {
          nativeBuildInputs = with pkgs; [
            rustc
            cargo
            rustfmt
            nixpkgs-fmt
            sccache
            clippy
            rust-analyzer
            bazel
            bazel-watcher
            cargo-outdated
            pre-commit

            # XXX: this doesn't quite let git operations work all the time,
            # sometimes we get git smudge errors if `git-crypt` is not also
            # installed in the host env
            git-crypt
          ] ++ lib.optional stdenv.isDarwin [
            darwin.apple_sdk.frameworks.SystemConfiguration
            iconv
          ];

          RUSTC_WRAPPER = "sccache";
          RUST_SRC_PATH = "${pkgs.rust.packages.stable.rustPlatform.rustLibSrc}";
        };
      }
    );
}
