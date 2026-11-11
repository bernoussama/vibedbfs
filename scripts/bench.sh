#!/usr/bin/env bash
# Before/after benchmark harness for dbfs. Needs no fio.
#
# Usage:
#   scripts/bench.sh [--label NAME] [--repeat N] [--workloads "a b c"] [--raw]
#
# Every workload run gets a fresh database and mount under .bench/ (disk-backed;
# never /tmp, which is tmpfs here). Results are appended as JSON lines to
# .bench/results/<label>.jsonl. The workloads are the bench_io ones (see
# examples/bench_io.rs). The cold workloads (seqread-cold, randread-cold,
# tree-cold) lay the data out, unmount, drop the page cache and remount before
# the timed part; dropping caches needs root or passwordless sudo.
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
cold_size=256M

while [[ $# -gt 0 ]]; do
  case "$1" in
    --label) label="${2:?}"; shift 2 ;;
    --repeat) repeat="${2:?}"; shift 2 ;;
    --workloads) workloads="${2:?}"; shift 2 ;;
    --raw) raw=1; shift ;;
    -h|--help) sed -n '2,${/^set -euo/q;p}' "$0"; exit 0 ;;
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
out="$bench_root/bench-out-$$.jsonl"
mkdir -p "$bench_root/results"
dbfs_pid=""
target_dir=""

unmount() {
  if [[ -n "$dbfs_pid" ]]; then
    fusermount3 -u "$run_dir/mnt" 2>/dev/null || fusermount3 -z -u "$run_dir/mnt" 2>/dev/null || true
    wait "$dbfs_pid" 2>/dev/null || true
    dbfs_pid=""
  fi
}
cleanup() {
  unmount
  rm -rf "$run_dir" "$out"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# Starts dbfs on the existing database in $run_dir (no-op with --raw).
mount_dbfs() {
  [[ "$raw" -eq 1 ]] && return 0
  "$DBFS_BIN" mount "$run_dir/db.sqlite" "$run_dir/mnt" >/dev/null 2>>"$run_dir/dbfs.log" &
  dbfs_pid=$!
  for _ in $(seq 300); do
    mountpoint -q "$run_dir/mnt" && break
    kill -0 "$dbfs_pid" 2>/dev/null || { cat "$run_dir/dbfs.log" >&2; exit 1; }
    sleep 0.05
  done
  mountpoint -q "$run_dir/mnt" || { echo "mount timed out" >&2; exit 1; }
}

# Empty database and mount point in $run_dir.
fresh_target() {
  unmount
  rm -rf "$run_dir"
  mkdir -p "$run_dir/mnt"
  target_dir="$run_dir/mnt"
  mount_dbfs
}

drop_caches() {
  sync
  if [[ "$(id -u)" -eq 0 ]]; then
    echo 3 > /proc/sys/vm/drop_caches
  elif sudo -n true 2>/dev/null; then
    sudo -n sh -c 'echo 3 > /proc/sys/vm/drop_caches'
  else
    echo "cold workloads need root or passwordless sudo to drop caches" >&2
    exit 1
  fi
}

# After the layout: unmount (which flushes), drop caches, remount the same database.
cold_reset() {
  unmount
  drop_caches
  mount_dbfs
}

# Runs one workload and prints bench_io's JSON lines. Not run in a subshell:
# the dbfs mount is started and stopped from here.
run_one() {
  local workload=$1 mode
  case "$workload" in
    seqread-cold|randread-cold)
      mode=seq
      [[ $workload == randread-cold ]] && mode=rand
      fresh_target
      "$bench_bin" layout "$target_dir" "$cold_size" >/dev/null
      cold_reset
      "$bench_bin" read-only "$target_dir" "$mode" "$cold_size"
      ;;
    tree-cold)
      fresh_target
      "$bench_bin" tree-layout "$target_dir" >/dev/null
      cold_reset
      "$bench_bin" tree-stat "$target_dir"
      "$bench_bin" tree-rm "$target_dir"
      ;;
    *)
      fresh_target
      "$bench_bin" "$workload" "$target_dir"
      ;;
  esac
}

target_name="dbfs:$DBFS_BIN"
[[ "$raw" -eq 1 ]] && target_name="raw:$(findmnt -n -o FSTYPE -T "$bench_root")"
echo "label=$label target=$target_name repeat=$repeat -> $results" >&2

for workload in $workloads; do
  for i in $(seq "$repeat"); do
    sync
    run_one "$workload" >"$out"
    unmount
    while IFS= read -r line; do
      [[ -n "$line" ]] || continue
      line="${line%\}},\"label\":\"$label\",\"run\":$i}"
      echo "$line" | tee -a "$results"
    done <"$out"
  done
done
