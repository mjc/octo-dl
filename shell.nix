{pkgs ? import <nixpkgs> {}}:
pkgs.mkShell {
  # nativeBuildInputs is usually what you want -- tools you need to run
  nativeBuildInputs = with pkgs.buildPackages; [
    git
    gh
    nodePackages.cspell

    cargo
    rustc
    rustfmt
    libclang
    rust-analyzer

    pkg-config
    openssl.dev

    # nix lang
    alejandra # nixos formatter
    nil # nix language server
  ];
  RUST_SRC_PATH = "${pkgs.rust.packages.stable.rustPlatform.rustLibSrc}";
  shellHook = ''
    export GH_CONFIG_DIR=$HOME/.config/gh/personal
    rustup override set nightly
  '';
}
