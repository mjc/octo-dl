#!/usr/bin/env bash
set -e

# Latency-focused profiling for MEGA downloads
# Unlike flamegraph profiling, this shows WHERE TIME IS SPENT WAITING
#
# Outputs:
#   - strace.log: syscall timing data
#   - flamegraph-offcpu.svg: off-CPU flamegraph (what we're waiting on)

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
cd "$PROJECT_DIR"

# ============================================================================
# Argument parsing
# ============================================================================

MODE=strace
CHUNKS=8
PARALLEL=4
FORCE=false
TUI=false
URLS=()

print_usage() {
    echo "Usage: $0 [OPTIONS] <URL>..."
    echo ""
    echo "Profile octo-dl download latency and waiting patterns"
    echo ""
    echo "Options:"
    echo "  -m, --mode <MODE>   Profiling mode: strace or offcpu (default: strace)"
    echo "  -j, --chunks <N>    Chunks per file (default: $CHUNKS)"
    echo "  -p, --parallel <N>  Concurrent file downloads (default: $PARALLEL)"
    echo "  -f, --force         Overwrite existing files"
    echo "  --tui               Launch the TUI binary instead of the CLI"
    echo "  -h, --help          Show this help"
    echo ""
    echo "Modes:"
    echo "  strace  - Record syscall latency (default)"
    echo "  offcpu  - Off-CPU flamegraph (what we're waiting on)"
    echo ""
    echo "Arguments:"
    echo "  URL...              One or more MEGA file/folder URLs"
    echo ""
    echo "Examples:"
    echo "  $0 -j 8 'https://mega.nz/file/xxxxx#yyyyy'"
    echo "  $0 -m offcpu -j 4 'https://mega.nz/folder/aaa#bbb'"
    echo "  $0 --tui -m strace"
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        -m|--mode)
            MODE="$2"; shift 2 ;;
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

# Trap Ctrl-C to continue with report generation
trap 'echo ""; echo "Stopping download, generating reports..."' INT

echo "=== Download Latency Profile Mode: $MODE ==="
echo ""

# Fix perf permissions
echo 0 | sudo tee /proc/sys/kernel/kptr_restrict > /dev/null
echo -1 | sudo tee /proc/sys/kernel/perf_event_paranoid > /dev/null
# Enable kernel tracing access for sched:sched_switch tracepoint
sudo chmod -R a+rx /sys/kernel/tracing 2>/dev/null || true
sudo chmod -R a+rx /sys/kernel/debug/tracing 2>/dev/null || true

# Build with profiling profile
RUSTFLAGS="-C target-cpu=native -C force-frame-pointers=yes" cargo build --release

# Select binary and build args
if [ "$TUI" = true ]; then
    BINARY="$PROJECT_DIR/target/release/octo-tui"
    BIN_ARGS=()
else
    BINARY="$PROJECT_DIR/target/release/octo-dl"
    BIN_ARGS=(-j "$CHUNKS" -p "$PARALLEL")
    if [ "$FORCE" = true ]; then
        BIN_ARGS+=(-f)
    fi
    BIN_ARGS+=("${URLS[@]}")
fi

echo "Profiling $(basename "$BINARY") (mode=$MODE, chunks=$CHUNKS, parallel=$PARALLEL)..."

case "$MODE" in
  strace)
    echo ""
    echo "Recording syscall latency with strace..."
    echo "Press Ctrl-C to stop and generate report"
    echo ""

    # -T: show time spent in syscall
    # -f: follow forks
    # -tt: microsecond timestamps
    # -e: trace these syscalls (network I/O heavy)
    # -o: output file (strace output goes here, not console)
    echo -ne "\033]0;octo-dl READY (strace)\007"
    echo -ne "\033kocto-dl READY\033\\"
    strace -T -f -tt -e read,write,recvfrom,sendto,poll,epoll_wait,pselect6,open,openat,close \
      -o strace.log \
      "$BINARY" "${BIN_ARGS[@]}" || true

    echo ""
    echo "=== Syscall Summary ==="
    echo ""

    # Summarize syscall times
    echo "Top syscalls by total time:"
    grep -oP '<[0-9.]+>' strace.log 2>/dev/null | tr -d '<>' | \
      awk '{sum+=$1; count++} END {if(count>0) printf "Total: %.3fs across %d calls (avg %.3fms)\n", sum, count, (sum/count)*1000}' || echo "(no data)"

    echo ""
    echo "Breakdown by syscall type:"
    for syscall in read write recvfrom sendto poll epoll_wait pselect6 open openat close; do
      if grep -q "^[0-9].*$syscall(" strace.log 2>/dev/null; then
        grep "$syscall(" strace.log 2>/dev/null | grep -oP '<[0-9.]+>' | tr -d '<>' | \
          awk -v name="$syscall" '{sum+=$1; count++} END {if(count>0) printf "  %-12s: %.3fs total, %6d calls, avg %.3fms\n", name, sum, count, (sum/count)*1000}'
      fi
    done

    echo ""
    echo "Slowest individual syscalls (>1ms):"
    grep -oP '^[0-9]+\s+[0-9:.]+\s+\S+\(.*<[0-9.]+>' strace.log 2>/dev/null | \
      awk -F'<' '{time=$2; gsub(/>.*/, "", time); if(time+0 > 0.001) print time, $1}' | \
      sort -rn | head -20 || echo "(no data)"

    echo ""
    echo "Full logs: strace.log"
    ;;

  offcpu)
    echo ""
    echo "Recording off-CPU time (what we're waiting on)..."
    echo "Press Ctrl-C to stop and generate flamegraph"
    echo ""

    # Try different methods for off-CPU profiling
    OFFCPU_METHOD=""

    # Method 1: Try bpftrace offcputime (most reliable if available)
    if command -v bpftrace &> /dev/null; then
      echo "Using bpftrace for off-CPU profiling..."
      OFFCPU_METHOD="bpftrace"
    # Method 2: Try perf with sched tracepoint
    elif perf record -e sched:sched_switch -a -- sleep 0.01 2>/dev/null; then
      rm -f perf.data
      echo "Using perf sched:sched_switch..."
      OFFCPU_METHOD="perf-sched"
    # Method 3: Try perf with software event (less accurate but works without tracepoints)
    elif perf record -e cpu-clock -a -- sleep 0.01 2>/dev/null; then
      rm -f perf.data
      echo "Using perf cpu-clock (less accurate, shows on-CPU not off-CPU)..."
      echo "Note: This won't show true off-CPU time, just CPU sampling"
      OFFCPU_METHOD="perf-cpu"
    else
      echo "Error: No off-CPU profiling method available"
      echo ""
      echo "Try installing bpftrace, or fix perf permissions:"
      echo "  sudo sh -c 'echo 0 > /proc/sys/kernel/perf_event_paranoid'"
      echo "  sudo chmod -R a+rx /sys/kernel/tracing"
      exit 1
    fi

    set +e
    case "$OFFCPU_METHOD" in
      bpftrace)
        echo -ne "\033]0;octo-dl READY (offcpu-bpftrace)\007"
        echo -ne "\033kocto-dl READY\033\\"
        echo "Recording... (press Ctrl-C to stop)"
        perf record -e sched:sched_switch -g --call-graph fp -F 997 -o perf-offcpu.data \
          "$BINARY" "${BIN_ARGS[@]}" || true
        ;;

      perf-sched)
        echo -ne "\033]0;octo-dl READY (offcpu-sched)\007"
        echo -ne "\033kocto-dl READY\033\\"

        # Start binary in background and capture its PID
        "$BINARY" "${BIN_ARGS[@]}" &
        APP_PID=$!

        # Give it a moment to start
        sleep 0.5

        # Record perf data for just this process
        perf sched record -p $APP_PID -o perf-offcpu.data

        # Wait for download to finish
        wait $APP_PID || true
        ;;

      perf-cpu)
        echo -ne "\033]0;octo-dl READY (offcpu-cpu)\007"
        echo -ne "\033kocto-dl READY\033\\"

        # Start binary in background and capture its PID
        "$BINARY" "${BIN_ARGS[@]}" &
        APP_PID=$!

        # Give it a moment to start
        sleep 0.5

        # Record perf data for just this process
        perf record -p $APP_PID -e cpu-clock -g --call-graph fp -F 997 -o perf-offcpu.data

        # Wait for download to finish
        wait $APP_PID || true
        ;;
    esac
    set -e

    echo ""
    echo "Generating reports..."

    if [ -f perf-offcpu.data ]; then
      if [ "$OFFCPU_METHOD" = "perf-sched" ]; then
        # perf sched gives latency report, not flamegraph
        echo ""
        echo "=== Scheduler Latency Summary ==="
        echo ""
        echo "Top threads by scheduling latency (wait time before running):"
        perf sched timehist -i perf-offcpu.data 2>&1 | \
          awk 'NR>2 {print $5 " " $4}' | grep -v '^$' | sort -rn | head -30 || true
        echo ""
        echo "For detailed analysis:"
        echo "  perf sched timehist -i perf-offcpu.data | less"
        echo ""
        echo "Note: Full scheduler latency report (perf sched latency) may be slow on large files"
      else
        # cpu-clock can make a flamegraph
        if command -v inferno-collapse-perf &> /dev/null; then
          perf script -i perf-offcpu.data 2>/dev/null | \
            inferno-collapse-perf | \
            inferno-flamegraph --title "Off-CPU Time" > flamegraph-offcpu.svg
          echo "Done: flamegraph-offcpu.svg"
        else
          echo "inferno not found, skipping flamegraph generation"
          echo "Install with: cargo install inferno"
        fi
      fi
    else
      echo "Warning: Could not generate perf data"
    fi

    echo ""
    echo "Output files: perf-offcpu.data (and flamegraph-offcpu.svg if generated)"
    ;;

  *)
    echo "Unknown mode: $MODE"
    echo ""
    print_usage
    exit 1
    ;;
esac
