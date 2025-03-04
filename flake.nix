{
  description = "A devShell example";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = {
    self,
    nixpkgs,
    rust-overlay,
    flake-utils,
    ...
  }:
    flake-utils.lib.eachDefaultSystem (
      system: let
        overlays = [(import rust-overlay)];
        pkgs = import nixpkgs {
          inherit system overlays;
        };
      in
        with pkgs; {
          devShells.default = mkShell {
            buildInputs = [
              openssl_3
              pkg-config
              eza
              fd
              (
                rust-bin.selectLatestNightlyWith (toolchain:
                  toolchain.default.override {
                    extensions = [
                      "rust-src"
                      "rust-analyzer"
                    ];
                  })
              )
            ];

            LD_LIBRARY_PATH = lib.makeLibraryPath [openssl_3];

            shellHook = ''
              alias ls=eza
              alias find=fd
            '';
          };
        }
    );
}
