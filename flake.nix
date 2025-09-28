{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.05";
    utils.url = "github:numtide/flake-utils";
    crane.url = "github:ipetkov/crane";
    rust-overlay.url = "github:oxalica/rust-overlay";
    fenix.url = "github:nix-community/fenix/monthly";
  };

  outputs = { self, nixpkgs, utils, crane, rust-overlay, fenix }:
    utils.lib.eachDefaultSystem (
      system:
      let
        overlays = [ (import rust-overlay) ];

        pkgs = import nixpkgs {
          inherit system overlays;
        };

        craneLib = (crane.mkLib pkgs).overrideToolchain (fenix.packages.${system}.fromToolchainFile {
          file = ./rust-toolchain.toml;
          sha256 = "sha256-+9FmLhAOezBZCOziO0Qct1NOrfpjNsXxc/8I0c7BdKE=";
        });

        commonArgs = {
          src = craneLib.cleanCargoSource ./.;
          strictDeps = true;

          buildInputs =
            [
              pkgs.apple-sdk
            ];
        };

        usbip-macos = craneLib.buildPackage (
          commonArgs
          // {
            cargoArtifacts = craneLib.buildDepsOnly commonArgs;
          }
        );
      in
      {
        formatter = pkgs.nixpkgs-fmt;

        checks = {
          inherit usbip-macos;
        };

        packages.default = usbip-macos;

        devShells.default = craneLib.devShell {
          checks = self.checks.${system};

          RUST_BACKTRACE = 1;
        };
      }
    );
}
