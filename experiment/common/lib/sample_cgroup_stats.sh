#!/bin/bash
# Per-second container-cgroup CPU and memory sampling, resolved from its host PID.
# Output schema stays compatible with sample_stats.sh; rss_mb contains cgroup
# memory.current (the container total, including all workers and charged cache).
set -uo pipefail

PID="$1"; OUT="$2"; MEM_LIMIT_RAW="${3:-}"; CPU_LIMIT="${4:-1}"

to_bytes() {
    local v="${1,,}"
    case "$v" in
        *k) echo $(( ${v%k} * 1024 ));;
        *m) echo $(( ${v%m} * 1024 * 1024 ));;
        *g) echo $(( ${v%g} * 1024 * 1024 * 1024 ));;
        *)  echo "$v";;
    esac
}

CGROUP_REL=$(awk -F: '$1 == "0" {print $3; exit}' "/proc/$PID/cgroup")
CGROUP_DIR="/sys/fs/cgroup${CGROUP_REL}"
[[ -r "$CGROUP_DIR/cpu.stat" && -r "$CGROUP_DIR/memory.current" ]] || exit 1

if [[ -n "$MEM_LIMIT_RAW" ]]; then
    MEM_LIMIT_B=$(to_bytes "$MEM_LIMIT_RAW")
else
    MEM_LIMIT_B=$(cat "$CGROUP_DIR/memory.max")
fi

read_cpu_usec() {
    awk '$1 == "usage_usec" {print $2; exit}' "$CGROUP_DIR/cpu.stat" 2>/dev/null
}

echo "elapsed_s,cpu_pct,cpu_lim_pct,cpu_time_s,rss_mb,mem_pct" > "$OUT"
T_START=$EPOCHREALTIME
PREV_USAGE=$(read_cpu_usec) || exit 0
PREV_T=$EPOCHREALTIME

while sleep 1; do
    USAGE=$(read_cpu_usec) || break
    MEM_B=$(cat "$CGROUP_DIR/memory.current" 2>/dev/null) || break
    NOW=$EPOCHREALTIME
    awk -v t0="$T_START" -v now="$NOW" -v pt="$PREV_T" \
        -v usage="$USAGE" -v prev_usage="$PREV_USAGE" \
        -v mem_b="$MEM_B" -v mem_limit_b="$MEM_LIMIT_B" -v ncpu="$CPU_LIMIT" \
        'BEGIN {
            dt = now - pt
            cpu_pct = dt > 0 ? (usage - prev_usage) / 1000000 / dt * 100 : 0
            printf "%.1f,%.1f,%.1f,%.2f,%.2f,%.2f\n",
                now - t0, cpu_pct, cpu_pct / ncpu, usage / 1000000,
                mem_b / 1048576, mem_b / mem_limit_b * 100
        }' >> "$OUT"
    PREV_USAGE="$USAGE"
    PREV_T="$NOW"
done

