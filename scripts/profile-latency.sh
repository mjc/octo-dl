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

MODE="${1:-strace}"
CHUNKS="${2:-8}"
shift 2 2>/dev/null || true
MEGA_URLS=("$@")

# Default URL if none provided
if [ ${#MEGA_URLS[@]} -eq 0 ]; then
    MEGA_URLS=("https://mega.nz/file/xxxxx#yyyyy")
fi

# Trap Ctrl-C to continue with report generation
trap 'echo ""; echo "Stopping download, generating reports..."' INT

echo "=== Download Latency Profile Mode: $MODE ==="
echo ""

if [ "$MODE" = "-h" ] || [ "$MODE" = "--help" ]; then
    echo "Usage: $0 [MODE] [CHUNKS] [MEGA_URLs...]"
    echo ""
    echo "Profile octo-dl download latency and waiting patterns"
    echo ""
    echo "Modes:"
    echo "  strace  - Record syscall latency (default)"
    echo "  offcpu  - Off-CPU flamegraph (what we're waiting on)"
    echo ""
    echo "Arguments:"
    echo "  CHUNKS     Number of parallel chunks (default: 8)"
    echo "  MEGA_URLs  One or more MEGA file/folder URLs"
    echo ""
    echo "Examples:"
    echo "  ./scripts/profile-latency.sh strace 8 'https://mega.nz/file/xxxxx#yyyyy'"
    echo "  ./scripts/profile-latency.sh offcpu 4 'https://mega.nz/folder/aaa#bbb' 'https://mega.nz/file/ccc#ddd'"
    echo ""
    exit 0
fi

# Fix perf permissions
echo 0 | sudo tee /proc/sys/kernel/kptr_restrict > /dev/null
echo -1 | sudo tee /proc/sys/kernel/perf_event_paranoid > /dev/null
# Enable kernel tracing access for sched:sched_switch tracepoint
sudo chmod -R a+rx /sys/kernel/tracing 2>/dev/null || true
sudo chmod -R a+rx /sys/kernel/debug/tracing 2>/dev/null || true

# Build with profiling profile
RUSTFLAGS="-C target-cpu=native -C force-frame-pointers=yes" cargo build --release

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
      "$PROJECT_DIR/target/release/octo-dl" -f -j "$CHUNKS" "${MEGA_URLS[@]}" || true

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
        # Bpftrace doesn't support filtering by PID easily, so we record everything
        echo -ne "\033]0;octo-dl READY (offcpu-bpftrace)\007"
        echo -ne "\033kocto-dl READY\033\\"
        echo "Recording... (press Ctrl-C to stop)"
        # This is a simplified approach - just run perf in the background
        perf record -e sched:sched_switch -g --call-graph fp -F 997 -o perf-offcpu.data \
          "$PROJECT_DIR/target/release/octo-dl" -f -j "$CHUNKS" "${MEGA_URLS[@]}" || true
        ;;

      perf-sched)
        # Use perf sched for proper scheduler analysis, filtering to just our process
        echo -ne "\033]0;octo-dl READY (offcpu-sched)\007"
        echo -ne "\033kocto-dl READY\033\\"

        # Start octo-dl in background and capture its PID
        "$PROJECT_DIR/target/release/octo-dl" -f -j "$CHUNKS" "${MEGA_URLS[@]}" &
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

        # Start octo-dl in background and capture its PID
        "$PROJECT_DIR/target/release/octo-dl" -f -j "$CHUNKS" "${MEGA_URLS[@]}" &
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
    echo "Usage: $0 [strace|offcpu] [CHUNKS] [MEGA_URLs...]"
    echo ""
    echo "Modes:"
    echo "  strace  - Record syscall latency (default)"
    echo "  offcpu  - Off-CPU flamegraph (tries perf)"
    exit 1
    ;;
esac
