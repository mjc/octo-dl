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
  tasks."check:check".exec =
    "CARGO_TARGET_DIR=target/devenv-check-check cargo check --all-targets --all-features --locked";
  tasks."check:clippy".exec =
    "CARGO_TARGET_DIR=target/devenv-check-clippy cargo clippy --all-targets --all-features --locked -- -D warnings";
  tasks."check:test".exec =
    "CARGO_TARGET_DIR=target/devenv-test-all cargo test --all-targets --all-features --locked";
  tasks."check:test:no-default".exec =
    "CARGO_TARGET_DIR=target/devenv-test-no-default cargo test --all-targets --no-default-features --locked";
  tasks."check:test:cli".exec =
    "CARGO_TARGET_DIR=target/devenv-test-cli cargo test --all-targets --no-default-features --features cli --locked";
  tasks."check:test:tui".exec =
    "CARGO_TARGET_DIR=target/devenv-test-tui cargo test --all-targets --no-default-features --features tui --locked";
  tasks."check:test:release:resume".exec =
    "CARGO_TARGET_DIR=target/devenv-test-release-resume cargo test --release --all-targets --all-features --locked resume";
  tasks."check:test:release:lifecycle".exec =
    "CARGO_TARGET_DIR=target/devenv-test-release-lifecycle cargo test --release --all-targets --all-features --locked lifecycle";
  tasks."check:dependencies".exec = "./scripts/check-dependencies.sh";
  tasks."check:flake".exec = "nix flake check --no-build --no-write-lock-file";
  tasks."check:all" = {
    # Independent prerequisites are scheduled in parallel by devenv. The
    # feature matrix uses test invocations because they compile the exact
    # targets they execute; the release checks stay focused on lifecycle and
    # resume regressions instead of repeating the full release suite.
    exec = "true";
    after = [
      "check:fmt"
      "check:check"
      "check:clippy"
      "check:test"
      "check:test:no-default"
      "check:test:cli"
      "check:test:tui"
      "check:test:release:resume"
      "check:test:release:lifecycle"
      "check:dependencies"
      "check:flake"
    ];
  };

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
