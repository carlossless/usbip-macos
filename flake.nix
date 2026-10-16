{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
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
          sha256 = "sha256-+xUr6Al1wZSqX8aEsq7DpuFOK4Ve/i+qTl+K5uWQ9GM=";
        });

        commonArgs = {
          src = craneLib.cleanCargoSource ./.;
          strictDeps = true;

          # .cargo/config.toml runs tests through sudo on macOS, which the build sandbox lacks.
          CARGO_TARGET_AARCH64_APPLE_DARWIN_RUNNER = "env";
          CARGO_TARGET_X86_64_APPLE_DARWIN_RUNNER = "env";

          buildInputs = pkgs.lib.optionals pkgs.stdenv.hostPlatform.isDarwin [
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
