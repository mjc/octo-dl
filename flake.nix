{
  description = "octo-dl - MEGA download manager";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay.url = "github:oxalica/rust-overlay";
    crane.url = "github:ipetkov/crane";
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
    crane,
    mega-rs,
    ...
  }:
    flake-utils.lib.eachDefaultSystem (
      system: let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [(import rust-overlay)];
        };
        cargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);
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

        cargoTargetEnvPrefix = pkgs.lib.toUpper (builtins.replaceStrings ["-"] ["_"] pkgs.stdenv.hostPlatform.config);
        cargoTargetLinkerEnv = "CARGO_TARGET_${cargoTargetEnvPrefix}_LINKER";
        cargoTargetRustflagsEnv = "CARGO_TARGET_${cargoTargetEnvPrefix}_RUSTFLAGS";
        linuxCcLinker = "${pkgs.stdenv.cc}/bin/cc";
        linuxMoldRustFlags = "-C link-arg=-fuse-ld=mold";

        # Crane setup with nightly rust
        rustNightly = pkgs.rust-bin.nightly.latest.default.override {
          extensions = ["rust-src"];
        };
        craneLib = (crane.mkLib pkgs).overrideToolchain rustNightly;

        # Source filtering - only include Rust-relevant files
        src = let
          # Include standard Rust files plus any extra assets
          filteredSrc = pkgs.lib.cleanSourceWith {
            src = ./.;
            filter = path: type: let
              pathString = toString path;
            in
              (craneLib.filterCargoSources path type)
              || builtins.match ".*\\.toml$" pathString != null
              || builtins.match ".*/src/tui/assets/.*" pathString != null;
          };
        in
          filteredSrc;

        # Common arguments shared between dep and source builds
        commonArgs = {
          inherit src;
          pname = "octo-dl";
          version = "0.1.0";
          strictDeps = true;

          nativeBuildInputs = [pkgs.pkg-config pkgs.mold];
          buildInputs = [pkgs.openssl];

          # Place mega-rs next to octo-dl so `path = "../mega-rs"` resolves
          postUnpack = ''
            cp -r ${mega-rs} mega-rs
            chmod -R u+w mega-rs
          '';
        }
        // pkgs.lib.optionalAttrs pkgs.stdenv.isLinux {
          "${cargoTargetLinkerEnv}" = linuxCcLinker;
          "${cargoTargetRustflagsEnv}" = linuxMoldRustFlags;
        };

        # Build only the cargo dependencies — cached when Cargo.lock is unchanged
        cargoArtifacts = craneLib.buildDepsOnly commonArgs;
      in {
        apps.default = flake-utils.lib.mkApp {
          drv = self.packages.${system}.octo-dl;
          exePath = "/bin/octo";
        };

        packages = {
          default = self.packages.${system}.octo-dl;

          octo-dl = craneLib.buildPackage (commonArgs
            // {
              inherit cargoArtifacts;

              postInstall = ''
                cat > "$out/bin/octo-tui" <<'EOF'
                #!/bin/sh
                attach_addr="127.0.0.1:9723"
                if [ "$#" -gt 0 ] && [ "''${1#-}" = "$1" ]; then
                  attach_addr="$1"
                  shift
                fi
                exec "@out@/bin/octo" --tui --tui-attach "$attach_addr" "$@"
                EOF
                substituteInPlace "$out/bin/octo-tui" --replace-fail "@out@" "$out"
                chmod +x "$out/bin/octo-tui"
              '';

              meta = with pkgs.lib; {
                description = cargoToml.package.description or "MEGA download manager with TUI, remote TUI attach, and headless service mode";
                homepage = "https://github.com/mjc/octo-dl";
                mainProgram = "octo";
              };
            });
        };

        # Clippy check as a separate cacheable derivation
        checks.clippy = craneLib.cargoClippy (commonArgs
          // {
            inherit cargoArtifacts;
            cargoClippyExtraArgs = "--all-targets";
          });

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
              cargo-bloat
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
              export "CARGO_TARGET_${cargoTargetEnvPrefix}_LINKER"="${pkgs.lib.optionalString pkgs.stdenv.isLinux linuxCcLinker}${pkgs.lib.optionalString (!pkgs.stdenv.isLinux) "${pkgs.stdenv.cc}/bin/cc"}"
              export "CARGO_TARGET_${cargoTargetEnvPrefix}_RUSTFLAGS"="-C target-cpu=native${pkgs.lib.optionalString pkgs.stdenv.isLinux " ${linuxMoldRustFlags}"}"
            ''
            + (
              if pkgs.stdenv.isLinux
              then ''
                export PATH=$PATH:''${RUSTUP_HOME:-~/.rustup}/toolchains/$RUSTC_VERSION-x86_64-unknown-linux-gnu/bin/
                export LD_LIBRARY_PATH="${pkgs.lib.makeLibraryPath (buildInputs ++ nativeBuildInputs)}''${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
              ''
              else ""
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
      nixosModules.default = {
        pkgs,
        lib,
        ...
      }: {
        imports = [./nixos-module.nix];
        services.octo-dl.package = lib.mkDefault self.packages.${pkgs.system}.octo-dl;
      };
      nixosModules.octo-dl = self.nixosModules.default;
    };
}
