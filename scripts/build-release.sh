#!/usr/bin/env bash
set -euo pipefail

# ============================================================================
# Octo-DL Release Build Script
# ============================================================================
# Builds cross-platform release binaries for Linux, Windows, and macOS.
#
# Usage:
# ./scripts/build-release.sh [VERSION]
#
# Cross-compilation strategy:
# - Linux (x86_64, aarch64): cargo-zigbuild with Zig linker
# - Windows (x86_64): cargo-zigbuild with Zig linker
# - macOS (universal): native cargo build with lipo (macOS host only)
# ============================================================================

# Determine version from argument or Cargo.toml
VERSION=${1:-$(grep '^version' Cargo.toml | head -1 | cut -d'"' -f2)}
BINARY_NAME="octo-dl"

# Detect host platform
HOST_OS=$(uname -s | tr '[:upper:]' '[:lower:]')
HOST_ARCH=$(uname -m)

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

# ============================================================================
# Helper Functions
# ============================================================================

log_info() {
	echo -e "${BLUE}ℹ${NC} $1"
}

log_success() {
	echo -e "${GREEN}✅${NC} $1"
}

log_warn() {
	echo -e "${YELLOW}⚠️${NC} $1"
}

log_error() {
	echo -e "${RED}❌${NC} $1"
}

build_linux_target() {
	local target=$1
	local arch=$2

	log_info "Building for $target..."
	cargo-zigbuild zigbuild --release --target "$target"
	tar -czf "release/v$VERSION/$BINARY_NAME-v$VERSION-$arch-linux.tar.gz" \
		-C "target/$target/release" "$BINARY_NAME"
	log_success "Built $arch Linux binary"
}

build_windows_target() {
	local target=$1
	local arch=$2

	log_info "Building for $target..."
	cargo-zigbuild zigbuild --release --target "$target"
	(cd "target/$target/release" && \
		zip "../../../release/v$VERSION/$BINARY_NAME-v$VERSION-$arch-windows.zip" \
		"$BINARY_NAME.exe")
	log_success "Built $arch Windows binary"
}

build_macos_universal() {
	log_info "Building macOS universal binaries..."

	# Build both architectures
	log_info "Building for x86_64-apple-darwin..."
	cargo build --release --target x86_64-apple-darwin

	log_info "Building for aarch64-apple-darwin..."
	cargo build --release --target aarch64-apple-darwin

	# Create universal binaries
	log_info "Creating universal binaries with lipo..."
	mkdir -p target/universal-apple-darwin/release

	lipo -create \
		"target/x86_64-apple-darwin/release/$BINARY_NAME" \
		"target/aarch64-apple-darwin/release/$BINARY_NAME" \
		-output "target/universal-apple-darwin/release/$BINARY_NAME"

	tar -czf "release/v$VERSION/$BINARY_NAME-v$VERSION-universal-darwin.tar.gz" \
		-C target/universal-apple-darwin/release "$BINARY_NAME"

	log_success "Built macOS universal binary"
}

# ============================================================================
# Main Build Process
# ============================================================================

echo "════════════════════════════════════════════════════════════════════════"
echo " Octo-DL Release Build"
echo " Version: $VERSION"
echo " Host: $HOST_OS-$HOST_ARCH"
echo "════════════════════════════════════════════════════════════════════════"
echo ""

# Enter cross-compilation environment if not already in it
if [ -z "${IN_NIX_SHELL:-}" ]; then
	log_info "Entering Nix cross-compilation environment..."
	exec nix develop .#cross -c "$0" "$@"
fi

log_info "Using Rust toolchain: $(rustc --version)"
echo ""

# Create release directory
mkdir -p "release/v$VERSION"

# ============================================================================
# Build Targets
# ============================================================================

# Linux targets
build_linux_target "x86_64-unknown-linux-gnu" "x86_64"
build_linux_target "aarch64-unknown-linux-gnu" "aarch64"

# Windows target
build_windows_target "x86_64-pc-windows-gnu" "x86_64"

# macOS targets (only on macOS host)
if [[ "$HOST_OS" == "darwin" ]]; then
	build_macos_universal
else
	echo ""
	log_warn "Skipping macOS targets (require macOS host for native compilation)"
	log_info "To build macOS binaries, run this script on a macOS machine."
fi

# ============================================================================
# Summary
# ============================================================================

echo ""
echo "════════════════════════════════════════════════════════════════════════"
log_success "Release build complete!"
echo ""
echo " Version: $VERSION"
echo " Output directory: release/v$VERSION/"
echo ""
echo "Built artifacts:"
find "release/v$VERSION/" -maxdepth 1 -type f -exec ls -lh {} \; | awk '{printf " • %s (%s)\n", $NF, $5}'
echo "════════════════════════════════════════════════════════════════════════"
