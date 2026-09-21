{
  description = "octo-dl - MEGA download manager";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay.url = "github:oxalica/rust-overlay";
    crane.url = "github:ipetkov/crane";
  };

  outputs = {
    self,
    nixpkgs,
    flake-utils,
    rust-overlay,
    crane,
    ...
  }:
    flake-utils.lib.eachDefaultSystem (
      system: let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [(import rust-overlay)];
        };
        cargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);

        cargoTargetEnvPrefix = pkgs.lib.toUpper (builtins.replaceStrings ["-"] ["_"] pkgs.stdenv.hostPlatform.config);
        cargoTargetLinkerEnv = "CARGO_TARGET_${cargoTargetEnvPrefix}_LINKER";
        cargoTargetRustflagsEnv = "CARGO_TARGET_${cargoTargetEnvPrefix}_RUSTFLAGS";
        linuxCcLinker = "${pkgs.stdenv.cc}/bin/cc";
        linuxMoldRustFlags = "-C link-arg=-fuse-ld=mold";

        # Keep the Crane build on the same stable compiler as devenv.
        rustStable = pkgs.rust-bin.stable."1.98.1".default.override {
          extensions = ["rust-src"];
        };
        craneLib = (crane.mkLib pkgs).overrideToolchain rustStable;

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
