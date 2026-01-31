{
  description = "Rust development environment";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = {
    self,
    nixpkgs,
    flake-utils,
  }:
    flake-utils.lib.eachDefaultSystem (
      system: let
        pkgs = nixpkgs.legacyPackages.${system};
        # Read the file relative to the flake's root
        overrides = builtins.fromTOML (builtins.readFile (self + "/rust-toolchain.toml"));
        libPath = with pkgs;
          lib.makeLibraryPath [
            # load external libraries that you need in your rust project here
          ];

        # Define reusable variables for paths
        glibIncludePaths = [
          ''-I${pkgs.glib.dev}/include/glib-2.0''
          ''-I${pkgs.glib.out}/lib/glib-2.0/include''
        ];

        clangIncludePaths = [
          ''-I${pkgs.llvmPackages_latest.libclang.lib}/lib/clang/${pkgs.llvmPackages_latest.libclang.version}/include''
        ];

        # glibc is Linux-only; include it only on Linux systems
        commonIncludePaths = if pkgs.stdenv.isLinux then [
          ''-I${pkgs.glibc.dev}/include''
        ] else [];
      in {
        devShells.default = pkgs.mkShell rec {
          nativeBuildInputs = [pkgs.pkg-config];
          buildInputs = with pkgs; [
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
          ] ++ (
            if pkgs.stdenv.isLinux then [
              # Linux-only profiling tools
              linuxPackages_latest.perf
              strace
            ] else []
          );

          RUSTC_VERSION = overrides.toolchain.channel;

          # https://github.com/rust-lang/rust-bindgen#environment-variables
          LIBCLANG_PATH = pkgs.lib.makeLibraryPath [pkgs.llvmPackages_latest.libclang.lib];

          shellHook = ''
            export PATH=$PATH:''${CARGO_HOME:-~/.cargo}/bin
          '' + (if pkgs.stdenv.isLinux then ''
            export PATH=$PATH:''${RUSTUP_HOME:-~/.rustup}/toolchains/$RUSTC_VERSION-x86_64-unknown-linux-gnu/bin/
            # Only set LD_LIBRARY_PATH for cargo/rustc, not globally (breaks system nix)
            export LD_LIBRARY_PATH="${pkgs.lib.makeLibraryPath (buildInputs ++ nativeBuildInputs)}''${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
          '' else ''
            # On macOS, rustup manages toolchains automatically
            true
          '');

          # Add precompiled library to rustc search path
          RUSTFLAGS = builtins.map (a: ''-L ${a}/lib'') [
            # add libraries here (e.g. pkgs.libvmi)
          ];

          # Add glibc, clang, glib, and other headers to bindgen search path
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
            # For Windows cross-compilation
            pkgsCross.mingwW64.stdenv.cc
          ];

          shellHook = ''
            export PATH=$PATH:''${CARGO_HOME:-~/.cargo}/bin

            # Clean environment - let Zig be the linker for cross-compilation
            unset CC
            unset CXX
            unset AR
            unset RANLIB

            # Set Zig cache to a writable location
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
    );
}
