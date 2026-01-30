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

# ============================================================================
# Argument parsing
# ============================================================================

CHUNKS=8
PARALLEL=4
FORCE=false
TUI=false
URLS=()

print_usage() {
    echo "Usage: $0 [OPTIONS] <URL>..."
    echo ""
    echo "Profile octo-dl download performance with flamegraph generation"
    echo ""
    echo "Options:"
    echo "  -j, --chunks <N>    Chunks per file (default: $CHUNKS)"
    echo "  -p, --parallel <N>  Concurrent file downloads (default: $PARALLEL)"
    echo "  -f, --force         Overwrite existing files"
    echo "  --tui               Launch the TUI binary instead of the CLI"
    echo "  -h, --help          Show this help"
    echo ""
    echo "Arguments:"
    echo "  URL...              One or more MEGA URLs (quote them!)"
    echo ""
    echo "Examples:"
    echo "  $0 -j 8 'https://mega.nz/folder/xxx#key1'"
    echo "  $0 --tui -j 4 -p 2 'https://mega.nz/folder/xxx#key1'"
    echo "  $0 -j 8 -f 'https://mega.nz/folder/xxx#key1' 'https://mega.nz/file/yyy#key2'"
    echo ""
    echo "The profile will:"
    echo "  1. Build release binary with frame pointers"
    echo "  2. Record CPU samples with perf"
    echo "  3. Generate flamegraph.svg showing where time is spent"
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        -j|--chunks)
            CHUNKS="$2"; shift 2 ;;
        -p|--parallel)
            PARALLEL="$2"; shift 2 ;;
        -f|--force)
            FORCE=true; shift ;;
        --tui)
            TUI=true; shift ;;
        -h|--help)
            print_usage; exit 0 ;;
        -*)
            echo "Unknown option: $1"; print_usage; exit 1 ;;
        *)
            URLS+=("$1"); shift ;;
    esac
done

if [ ${#URLS[@]} -eq 0 ] && [ "$TUI" = false ]; then
    echo "Error: No URLs provided"
    echo ""
    print_usage
    exit 1
fi

# Build with native CPU + frame pointers
RUSTFLAGS="-C target-cpu=native -C force-frame-pointers=yes" cargo build --release

# Select binary and build args
if [ "$TUI" = true ]; then
    BINARY="$PROJECT_DIR/target/release/octo-tui"
    # TUI doesn't take CLI args for URLs — they're entered interactively
    BIN_ARGS=()
else
    BINARY="$PROJECT_DIR/target/release/octo-dl"
    BIN_ARGS=(-j "$CHUNKS" -p "$PARALLEL")
    if [ "$FORCE" = true ]; then
        BIN_ARGS+=(-f)
    fi
    BIN_ARGS+=("${URLS[@]}")
fi

echo "Profiling with $(basename "$BINARY") (chunks=$CHUNKS, parallel=$PARALLEL)..."

# Record using frame pointers (not DWARF)
# Disable set -e for perf since Ctrl-C causes non-zero exit
set +e
echo -ne "\033]0;octo-dl READY (profile CPU)\007"  # terminal title
echo -ne "\033kocto-dl READY\033\\"                 # tmux window name
perf record -g --call-graph fp -F 997 \
  "$BINARY" "${BIN_ARGS[@]}"
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
