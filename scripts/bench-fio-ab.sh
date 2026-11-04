#!/usr/bin/env bash
# Interleaved fio A/B comparison of dbfs binaries, using the workloads from
# docs/2026-05-03-profiling-results.md. Each run uses a fresh database under
# .bench/fio/ (disk-backed). fio JSON goes to .bench/fio/<label>-<job>-<run>.json.
#
#   scripts/bench-fio-ab.sh [--repeat N] LABEL=BINARY [LABEL=BINARY ...]
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
out="$repo_root/.bench/fio"
repeat=2
if [[ "${1:-}" == "--repeat" ]]; then repeat="$2"; shift 2; fi
[[ $# -ge 1 ]] || { sed -n 2,7p "$0"; exit 2; }
mkdir -p "$out"

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
