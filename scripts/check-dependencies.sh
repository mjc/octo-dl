#!/usr/bin/env bash
set -euo pipefail

cargo-deny --all-features --locked check
cargo audit --ignore RUSTSEC-2023-0071
