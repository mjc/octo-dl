{ pkgs, lib, ... }:

let
  linux = pkgs.stdenv.hostPlatform.isLinux;
  targetPrefix = lib.toUpper (builtins.replaceStrings ["-"] ["_"] pkgs.stdenv.hostPlatform.config);
  targetLinker = "CARGO_TARGET_${targetPrefix}_LINKER";
  targetRustflags = "CARGO_TARGET_${targetPrefix}_RUSTFLAGS";
  linuxLinker = "${pkgs.stdenv.cc}/bin/cc";
  linuxRustflags = "-C target-cpu=native -C link-arg=-fuse-ld=mold";
  commonRustflags = "-C target-cpu=native";

  glibIncludePaths = [
    "-I${pkgs.glib.dev}/include/glib-2.0"
    "-I${pkgs.glib.out}/lib/glib-2.0/include"
  ];
  clangIncludePaths = [
    "-I${pkgs.llvmPackages_21.libclang.lib}/lib/clang/${pkgs.llvmPackages_21.libclang.version}/include"
  ];
  commonIncludePaths = lib.optionals linux ["-I${pkgs.glibc.dev}/include"];
  bindgenArgs = lib.concatStringsSep " " (commonIncludePaths ++ clangIncludePaths ++ glibIncludePaths);

  basePackages = with pkgs; [
    bc
    cargo-audit
    cargo-bloat
    cargo-deny
    clang
    gh
    gnuplot
    llvmPackages.bintools
    openssl
    openssl.dev
    par2cmdline
    pkg-config
    sccache
    xxd
  ];
in
{
  languages.rust = {
    enable = true;
    toolchainFile = ./rust-toolchain.toml;
    mold.enable = linux;
  };

  packages = basePackages ++ lib.optionals linux [
    pkgs.linuxPackages_latest.perf
    pkgs.mold
    pkgs.strace
  ];

  env = {
    CARGO_TERM_COLOR = "always";
    LIBCLANG_PATH = lib.makeLibraryPath [pkgs.llvmPackages_21.libclang.lib];
    LIBRARY_PATH = lib.makeLibraryPath (lib.optional (!linux) pkgs.libiconv);
    RUSTC_VERSION = "1.98.1";
    RUSTC_WRAPPER = "${pkgs.sccache}/bin/sccache";
    BINDGEN_EXTRA_CLANG_ARGS = bindgenArgs;
  } // {
    "${targetLinker}" = linuxLinker;
    "${targetRustflags}" = if linux then linuxRustflags else commonRustflags;
  };

  tasks."check:fmt".exec = "cargo fmt --all -- --check";
  tasks."check:check".exec = "cargo check --all-targets --all-features --locked";
  tasks."check:test".exec = "cargo test --all-targets --all-features --locked";
  tasks."check:dependencies".exec = "./scripts/check-dependencies.sh";
  tasks."check:flake".exec = "nix flake check --no-build --no-write-lock-file";
  tasks."check:all".exec = ''
    cargo fmt --all -- --check
    cargo check --all-targets --all-features --locked
    cargo test --all-targets --all-features --locked
    ./scripts/check-dependencies.sh
    nix flake check --no-build --no-write-lock-file
  '';

  profiles.cross.module = {
    packages = with pkgs; [
      cargo-zigbuild
      pkgsCross.mingwW64.stdenv.cc
      rustup
      zig
    ];

    env.OCTO_DEVENV_CROSS = "1";

    enterShell = ''
      unset CC
      unset CXX
      unset AR
      unset RANLIB

      export ZIG_GLOBAL_CACHE_DIR="''${ZIG_GLOBAL_CACHE_DIR:-$HOME/.cache/zig}"
      export ZIG_LOCAL_CACHE_DIR="''${ZIG_LOCAL_CACHE_DIR:-$PWD/.zig-cache}"

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
