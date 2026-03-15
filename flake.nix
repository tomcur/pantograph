{
  description = "Wait-free channels";
  inputs.flake-utils.url = "github:numtide/flake-utils";
  inputs.rust-overlay.url = "github:oxalica/rust-overlay";
  outputs = { nixpkgs, flake-utils, rust-overlay, ... }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs {
          inherit system overlays;
        };
      in
      {
        devShell = pkgs.mkShell {
          packages = with pkgs; [
            (rust-bin.nightly.latest.minimal.override {
              extensions = [ "rust-src" "rustfmt" "rust-analyzer" "miri" "clippy" "llvm-tools" ];
            })
            typos
          ];
        };
      });
}
