#!/usr/bin/env bash
# Interleaved fio A/B comparison of dbfs binaries, using the workloads from
# docs/2026-05-03-profiling-results.md. Each run uses a fresh database under
# .bench/fio/ (disk-backed). fio JSON goes to .bench/fio/<label>-<job>-<run>.json.
# The output directory is refused if it is on tmpfs or ramfs, unless
# --allow-tmpfs (or DBFS_BENCH_ALLOW_TMPFS=1) is given. bench-fio.sh checks that
# each mount is FUSE.
#
#   scripts/bench-fio-ab.sh [--repeat N] [--allow-tmpfs] LABEL=BINARY [LABEL=BINARY ...]
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
out="$repo_root/.bench/fio"
repeat=2
allow_tmpfs="${DBFS_BENCH_ALLOW_TMPFS:-0}"
while [[ $# -gt 0 ]]; do
  case "$1" in
    --repeat) repeat="${2:?--repeat requires a value}"; shift 2 ;;
    --allow-tmpfs) allow_tmpfs=1; shift ;;
    *) break ;;
  esac
done
[[ $# -ge 1 ]] || { echo "usage: scripts/bench-fio-ab.sh [--repeat N] [--allow-tmpfs] LABEL=BINARY [LABEL=BINARY ...]" >&2; exit 2; }
mkdir -p "$out"

fstype="$(findmnt -n -o FSTYPE -T "$out")"
case "$fstype" in
  tmpfs|ramfs)
    if [[ "$allow_tmpfs" != 1 ]]; then
      echo "refusing: $out is on $fstype, which is RAM, not disk. Pass --allow-tmpfs to benchmark RAM on purpose." >&2
      exit 1
    fi
    echo "warning: $out is on $fstype; this measures RAM" >&2
    ;;
esac
export DBFS_BENCH_ALLOW_TMPFS="$allow_tmpfs"

# name | bench-fio.sh arguments
jobs=(
  "randwrite-nofsync|--job randwrite --size 256M --bs 4k --fsync 0"
  "randwrite-fsync|--job randwrite --size 64M --bs 4k --fsync 1"
  "randread|--job randread --size 256M --bs 4k --fsync 0"
  "seqwrite|--job write --size 512M --bs 128k --fsync 0"
)

for run in $(seq "$repeat"); do
  for job in "${jobs[@]}"; do
    name="${job%%|*}"
    read -r -a args <<<"${job#*|}"
    for pair in "$@"; do
      label="${pair%%=*}"
      bin="${pair#*=}"
      db="$out/db.sqlite"
      rm -f "$db" "$db-wal" "$db-shm"
      json="$out/$label-$name-$run.json"
      sync
      DBFS_BIN="$bin" "$repo_root/scripts/bench-fio.sh" "${args[@]}" \
        --database "$db" --mountpoint "$out/mnt" \
        -- --output-format=json --output="$json" >/dev/null
      echo "$label $name run=$run -> $json"
    done
  done
done
rm -f "$out/db.sqlite" "$out/db.sqlite-wal" "$out/db.sqlite-shm"
