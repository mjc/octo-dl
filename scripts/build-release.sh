#!/usr/bin/env bash
set -euo pipefail

VERSION="${1:-$(grep '^version' Cargo.toml | head -1 | cut -d'"' -f2)}"
BINARY_NAME="octo-dl"

echo "Building octo-dl v${VERSION}"
echo "================================"

# Targets to build
TARGETS=(
    "x86_64-unknown-linux-gnu"
    "aarch64-unknown-linux-gnu"
    "x86_64-pc-windows-gnu"
)

# Check for cargo-zigbuild
if ! command -v cargo-zigbuild &> /dev/null; then
    echo "Error: cargo-zigbuild not found"
    echo "Install with: cargo install cargo-zigbuild"
    echo "Or enter nix cross shell: nix develop .#cross"
    exit 1
fi

# Build each target
for target in "${TARGETS[@]}"; do
    echo ""
    echo "Building for ${target}..."
    echo "----------------------------------------"

    if cargo zigbuild --release --target "${target}"; then
        # Determine binary extension
        if [[ "${target}" == *"windows"* ]]; then
            binary="target/${target}/release/${BINARY_NAME}.exe"
        else
            binary="target/${target}/release/${BINARY_NAME}"
        fi

        if [[ -f "${binary}" ]]; then
            size=$(du -h "${binary}" | cut -f1)
            echo "Success: ${binary} (${size})"
        else
            echo "Warning: Binary not found at ${binary}"
        fi
    else
        echo "Failed to build for ${target}"
    fi
done

echo ""
echo "================================"
echo "Build Summary"
echo "================================"

# Show all built binaries
for target in "${TARGETS[@]}"; do
    if [[ "${target}" == *"windows"* ]]; then
        binary="target/${target}/release/${BINARY_NAME}.exe"
    else
        binary="target/${target}/release/${BINARY_NAME}"
    fi

    if [[ -f "${binary}" ]]; then
        size=$(du -h "${binary}" | cut -f1)
        echo "  ${target}: ${size}"
    else
        echo "  ${target}: NOT BUILT"
    fi
done
