{
  description = "octo-dl - MEGA download manager";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay.url = "github:oxalica/rust-overlay";
    mega-rs = {
      url = "github:mjc/mega-rs/parallel-download";
      flake = false;
    };
  };

  outputs = {
    self,
    nixpkgs,
    flake-utils,
    rust-overlay,
    mega-rs,
    ...
  }:
    flake-utils.lib.eachDefaultSystem (
      system: let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [(import rust-overlay)];
        };
        overrides = builtins.fromTOML (builtins.readFile (self + "/rust-toolchain.toml"));
        libPath = with pkgs;
          lib.makeLibraryPath [];

        # glib include paths for bindgen
        glibIncludePaths = [
          ''-I${pkgs.glib.dev}/include/glib-2.0''
          ''-I${pkgs.glib.out}/lib/glib-2.0/include''
        ];

        clangIncludePaths = [
          ''-I${pkgs.llvmPackages_latest.libclang.lib}/lib/clang/${pkgs.llvmPackages_latest.libclang.version}/include''
        ];

        commonIncludePaths =
          if pkgs.stdenv.isLinux
          then [''-I${pkgs.glibc.dev}/include'']
          else [];

        cargoTargetEnvPrefix = pkgs.lib.toUpper (builtins.replaceStrings ["-"] ["_"] pkgs.rust.toRustTargetSpec pkgs.stdenv.hostPlatform);
      in {
        packages = {
          default = self.packages.${system}.octo-dl;

          octo-dl = let
            rustNightly = pkgs.rust-bin.nightly.latest.default.override {
              extensions = ["rust-src"];
            };
            rustPlatform = pkgs.makeRustPlatform {
              cargo = rustNightly;
              rustc = rustNightly;
            };
          in
            rustPlatform.buildRustPackage {
              pname = "octo-dl";
              version = "0.1.0";
              src = ./.;

              # Place mega-rs next to octo-dl so `path = "../mega-rs"` resolves
              postUnpack = ''
                cp -r ${mega-rs} mega-rs
                chmod -R u+w mega-rs
              '';

              cargoHash = "sha256-ncbjDEeH2lCY8aThCz1lMU2X02F9fLw+k246aP6/uFY=";

              nativeBuildInputs = [pkgs.pkg-config];
              buildInputs = [pkgs.openssl];

              meta = with pkgs.lib; {
                description = "MEGA download manager with TUI and headless service mode";
                homepage = "https://github.com/mjc/octo-dl";
                mainProgram = "octo";
              };
            };
        };

        devShells.default = pkgs.mkShell rec {
          nativeBuildInputs = [pkgs.pkg-config];
          buildInputs = with pkgs;
            [
              clang
              llvmPackages.bintools
              rustup
              openssl
              openssl.dev
              pkg-config
              par2cmdline
              xxd
              gh
              gnuplot
              bc
              sccache
            ]
            ++ (
              if pkgs.stdenv.isLinux
              then [
                linuxPackages_latest.perf
                strace
                mold
              ]
              else []
            );

          RUSTC_VERSION = overrides.toolchain.channel;
          LIBCLANG_PATH = pkgs.lib.makeLibraryPath [pkgs.llvmPackages_latest.libclang.lib];

          shellHook =
            ''
              export PATH=$PATH:''${CARGO_HOME:-~/.cargo}/bin
              export RUSTC_WRAPPER="${pkgs.sccache}/bin/sccache"
              export "CARGO_TARGET_''${cargoTargetEnvPrefix}_LINKER"="${pkgs.lib.optionalString pkgs.stdenv.isLinux "${pkgs.mold}/bin/mold -run "}${pkgs.stdenv.cc}/bin/cc"
              export "CARGO_TARGET_''${cargoTargetEnvPrefix}_RUSTFLAGS"="-C target-cpu=native"
            ''
            + (
              if pkgs.stdenv.isLinux
              then ''
                export PATH=$PATH:''${RUSTUP_HOME:-~/.rustup}/toolchains/$RUSTC_VERSION-x86_64-unknown-linux-gnu/bin/
                export LD_LIBRARY_PATH="${pkgs.lib.makeLibraryPath (buildInputs ++ nativeBuildInputs)}''${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
              ''
              else ''
                true
              ''
            );

          RUSTFLAGS = builtins.map (a: ''-L ${a}/lib'') [];

          BINDGEN_EXTRA_CLANG_ARGS =
            (builtins.map (a: ''-I${a}/include'') commonIncludePaths)
            ++ clangIncludePaths
            ++ glibIncludePaths;
        };

        # Cross-compilation shell for release builds
        devShells.cross = pkgs.mkShell {
          nativeBuildInputs = with pkgs; [
            rustup
            cargo-zigbuild
            zig
            pkg-config
            pkgsCross.mingwW64.stdenv.cc
          ];

          shellHook = ''
            export PATH=$PATH:''${CARGO_HOME:-~/.cargo}/bin

            unset CC
            unset CXX
            unset AR
            unset RANLIB

            export ZIG_GLOBAL_CACHE_DIR="$HOME/.cache/zig"
            export ZIG_LOCAL_CACHE_DIR="$PWD/.zig-cache"

            echo "Cross-compilation environment ready"
            echo "Available targets:"
            echo "  - x86_64-unknown-linux-gnu"
            echo "  - aarch64-unknown-linux-gnu"
            echo "  - x86_64-pc-windows-gnu"
            echo ""
            echo "Build with: cargo zigbuild --release --target <target>"
            echo "Or run: ./scripts/build-release.sh <version>"
          '';
        };
      }
    )
    // {
      # NixOS module (system-independent, outside eachDefaultSystem)
      nixosModules.default = import ./nixos-module.nix;
    };
}
