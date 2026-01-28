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
CHUNKS="${1:-8}"
shift 2>/dev/null || true
URLS=("$@")

if [ "$CHUNKS" = "-h" ] || [ "$CHUNKS" = "--help" ]; then
    echo "Usage: $0 [CHUNKS] [URL...]"
    echo ""
    echo "Profile octo-dl download performance"
    echo ""
    echo "Arguments:"
    echo "  CHUNKS      Number of parallel chunks (default: 8)"
    echo "  URL...      One or more MEGA URLs (quote them!)"
    echo ""
    echo "Example:"
    echo "  ./scripts/profile.sh 8 'https://mega.nz/folder/xxx#key1' 'https://mega.nz/folder/yyy#key2'"
    echo ""
    echo "The profile will:"
    echo "  1. Build release binary with frame pointers"
    echo "  2. Record CPU samples with perf"
    echo "  3. Generate flamegraph.svg showing where time is spent"
    exit 0
fi

if [ ${#URLS[@]} -eq 0 ]; then
    echo "Error: No URLs provided"
    echo "Usage: $0 [CHUNKS] [URL...]"
    exit 1
fi

echo "Profiling ${#URLS[@]} URL(s) with $CHUNKS chunks..."

# Record using frame pointers (not DWARF)
# Disable set -e for perf since Ctrl-C causes non-zero exit
set +e
echo -ne "\033]0;octo-dl READY (profile CPU)\007"  # terminal title
echo -ne "\033kocto-dl READY\033\\"                 # tmux window name
perf record -g --call-graph fp -F 997 \
  "$PROJECT_DIR/target/release/octo-dl" -f -j "$CHUNKS" "${URLS[@]}"
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
