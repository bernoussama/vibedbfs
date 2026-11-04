#!/usr/bin/env bash
# Before/after benchmark harness for dbfs. Needs no fio.
#
# Usage:
#   scripts/bench.sh [--label NAME] [--repeat N] [--workloads "a b c"] [--raw]
#
# Every workload run gets a fresh database and mount under .bench/ (disk-backed;
# never /tmp, which is tmpfs here). Results are appended as JSON lines to
# .bench/results/<label>.jsonl.
#
# Environment:
#   DBFS_BIN   dbfs binary to benchmark (default: target/release/dbfs, rebuilt)
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
bench_root="$repo_root/.bench"
label="current"
repeat=3
workloads="seqwrite randwrite randwrite-fsync seqread randread metadata bigdir"
raw=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --label) label="${2:?}"; shift 2 ;;
    --repeat) repeat="${2:?}"; shift 2 ;;
    --workloads) workloads="${2:?}"; shift 2 ;;
    --raw) raw=1; shift ;;
    -h|--help) sed -n 2,14p "$0"; exit 0 ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done

cargo_cmd=(cargo)
command -v cargo >/dev/null 2>&1 || cargo_cmd=("$repo_root/scripts/cargo-docker.sh")
"${cargo_cmd[@]}" build --release --quiet --manifest-path "$repo_root/Cargo.toml" --example bench_io
bench_bin="$repo_root/target/release/examples/bench_io"
if [[ -z "${DBFS_BIN:-}" ]]; then
  "${cargo_cmd[@]}" build --release --quiet --manifest-path "$repo_root/Cargo.toml"
  DBFS_BIN="$repo_root/target/release/dbfs"
fi

run_dir="$bench_root/run"
results="$bench_root/results/$label.jsonl"
mkdir -p "$bench_root/results"
dbfs_pid=""

unmount() {
  if [[ -n "$dbfs_pid" ]]; then
    fusermount3 -u "$run_dir/mnt" 2>/dev/null || fusermount3 -z -u "$run_dir/mnt" 2>/dev/null || true
    wait "$dbfs_pid" 2>/dev/null || true
    dbfs_pid=""
  fi
}
trap 'unmount; rm -rf "$run_dir"' EXIT INT TERM

fresh_target() {
  unmount
  rm -rf "$run_dir"
  mkdir -p "$run_dir/mnt"
  target_dir="$run_dir/mnt"
  [[ "$raw" -eq 1 ]] && return
  "$DBFS_BIN" mount "$run_dir/db.sqlite" "$run_dir/mnt" >/dev/null 2>"$run_dir/dbfs.log" &
  dbfs_pid=$!
  for _ in $(seq 300); do
    mountpoint -q "$run_dir/mnt" && break
    kill -0 "$dbfs_pid" 2>/dev/null || { cat "$run_dir/dbfs.log" >&2; exit 1; }
    sleep 0.05
  done
  mountpoint -q "$run_dir/mnt" || { echo "mount timed out" >&2; exit 1; }
}

target_name="dbfs:$DBFS_BIN"
[[ "$raw" -eq 1 ]] && target_name="raw:$(findmnt -n -o FSTYPE -T "$bench_root")"
echo "label=$label target=$target_name repeat=$repeat -> $results" >&2

for workload in $workloads; do
  for i in $(seq "$repeat"); do
    sync
    fresh_target
    line="$("$bench_bin" "$workload" "$target_dir")"
    unmount
    line="${line%\}},\"label\":\"$label\",\"run\":$i}"
    echo "$line" | tee -a "$results"
  done
done
