#!/usr/bin/env bash
set -euo pipefail

cargo-deny --all-features --locked check
# See deny.toml: the MEGA TLS client rejects private-key operations.
cargo audit --ignore RUSTSEC-2023-0071
