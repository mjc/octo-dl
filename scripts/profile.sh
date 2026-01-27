#!/usr/bin/env bash
set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
cd "$PROJECT_DIR"

# Trap Ctrl-C and continue with flamegraph generation
trap 'echo ""; echo "Stopping perf record, generating flamegraph..."' INT

# Check deps
if ! command -v inferno-collapse-perf &> /dev/null; then
    echo "Installing inferno..."
    cargo install inferno
fi

# Fix perf permissions
echo 0 | sudo tee /proc/sys/kernel/kptr_restrict > /dev/null
echo -1 | sudo tee /proc/sys/kernel/perf_event_paranoid > /dev/null

# Build with native CPU + frame pointers
RUSTFLAGS="-C target-cpu=native -C force-frame-pointers=yes" cargo build --release

# Usage
MEGA_URL="${1:-https://mega.nz/file/xxxxx#yyyyy}"
CHUNKS="${2:-8}"

if [ "$MEGA_URL" = "-h" ] || [ "$MEGA_URL" = "--help" ]; then
    echo "Usage: $0 [MEGA_URL] [CHUNKS]"
    echo ""
    echo "Profile octo-dl download performance"
    echo ""
    echo "Arguments:"
    echo "  MEGA_URL    MEGA file URL (default: $MEGA_URL)"
    echo "  CHUNKS      Number of parallel chunks (default: $CHUNKS)"
    echo ""
    echo "Example:"
    echo "  ./scripts/profile.sh 'https://mega.nz/file/xxxxx#yyyyy' 8"
    echo ""
    echo "The profile will:"
    echo "  1. Build release binary with frame pointers"
    echo "  2. Record CPU samples with perf"
    echo "  3. Generate flamegraph.svg showing where time is spent"
    exit 0
fi

# Record using frame pointers (not DWARF)
# Disable set -e for perf since Ctrl-C causes non-zero exit
set +e
echo -ne "\033]0;octo-dl READY (profile CPU)\007"  # terminal title
echo -ne "\033kocto-dl READY\033\\"                 # tmux window name
perf record -g --call-graph fp -F 997 \
  "$PROJECT_DIR/target/release/octo-dl" -f -j "$CHUNKS" "$MEGA_URL"
set -e

echo ""
echo "Generating flamegraph from perf.data..."

# Generate flamegraph
if [ ! -f perf.data ]; then
    echo "Error: perf.data not found"
    exit 1
fi

perf script 2>/dev/null | inferno-collapse-perf | inferno-flamegraph > flamegraph.svg

echo "Done: flamegraph.svg"
echo ""
echo "Open with: firefox flamegraph.svg (or your favorite SVG viewer)"
