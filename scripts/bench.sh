#!/usr/bin/env bash
# Before/after benchmark harness for dbfs. Needs no fio.
#
# Usage:
#   scripts/bench.sh [--label NAME] [--repeat N] [--workloads "a b c"] [--raw] [--allow-tmpfs]
#
# Every workload run gets a fresh database and mount under .bench/ (disk-backed;
# never /tmp, which is tmpfs on some machines). The harness refuses to run when
# .bench/ is on tmpfs or ramfs, unless --allow-tmpfs is given. Results are
# appended as JSON lines to .bench/results/<label>.jsonl. Each line records the
# target path (target), the filesystem type mounted there (target_fstype) and the
# filesystem that holds the database (storage_fstype), so a mislabelled run shows.
#
# The workloads are the bench_io ones (see examples/bench_io.rs). The cold
# workloads (seqread-cold, randread-cold, tree-cold) lay the data out, unmount,
# drop the page cache and remount before the timed part; dropping caches needs
# root or passwordless sudo.
#
# Environment:
#   DBFS_BIN                  dbfs binary to benchmark (default: target/release/dbfs, rebuilt)
#   DBFS_BENCH_ALLOW_TMPFS=1  same as --allow-tmpfs
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
bench_root="$repo_root/.bench"
label="current"
repeat=3
workloads="seqwrite randwrite randwrite-fsync seqread randread metadata bigdir"
raw=0
allow_tmpfs="${DBFS_BENCH_ALLOW_TMPFS:-0}"
cold_size=256M

while [[ $# -gt 0 ]]; do
  case "$1" in
    --label) label="${2:?}"; shift 2 ;;
    --repeat) repeat="${2:?}"; shift 2 ;;
    --workloads) workloads="${2:?}"; shift 2 ;;
    --raw) raw=1; shift ;;
    --allow-tmpfs) allow_tmpfs=1; shift ;;
    -h|--help) sed -n '2,${/^set -euo/q;p}' "$0"; exit 0 ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done

run_dir="$bench_root/run"
results="$bench_root/results/$label.jsonl"
out="$bench_root/bench-out-$$.jsonl"
dbfs_pid=""
target_dir=""
target_fstype=""
storage_fstype=""

# Prints the filesystem type of the nearest existing directory at or above $1.
existing_fstype() {
  local path="$1"
  while [[ ! -e "$path" ]]; do
    path="$(dirname "$path")"
  done
  findmnt -n -o FSTYPE -T "$path"
}

# Refuses a directory on tmpfs or ramfs unless --allow-tmpfs was given.
check_disk_backed() {
  local dir="$1" fstype
  fstype="$(existing_fstype "$dir")"
  case "$fstype" in
    tmpfs|ramfs)
      if [[ "$allow_tmpfs" != 1 ]]; then
        echo "refusing: $dir is on $fstype, which is RAM, not disk. Pass --allow-tmpfs to benchmark RAM on purpose." >&2
        exit 1
      fi
      echo "warning: $dir is on $fstype; this measures RAM" >&2
      ;;
  esac
}

# Prints the filesystem type mounted exactly at $1, or nothing.
mounted_fstype() {
  findmnt -n -o FSTYPE --mountpoint "$1" 2>/dev/null | tail -n 1 || true
}

unmount() {
  if [[ -n "$dbfs_pid" ]]; then
    fusermount3 -u "$run_dir/mnt" 2>/dev/null || fusermount3 -z -u "$run_dir/mnt" 2>/dev/null || true
    wait "$dbfs_pid" 2>/dev/null || true
    dbfs_pid=""
  fi
}

cleanup() {
  set +e
  unmount
  # rm -rf would walk into a mount that is still there, so keep the directory instead.
  if [[ -n "$(mounted_fstype "$run_dir/mnt")" ]]; then
    echo "not removing $run_dir: $run_dir/mnt is still mounted" >&2
  else
    rm -rf "$run_dir"
  fi
  rm -f "$out"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

mkdir -p "$bench_root"
check_disk_backed "$bench_root"

cargo_cmd=(cargo)
command -v cargo >/dev/null 2>&1 || cargo_cmd=("$repo_root/scripts/cargo-docker.sh")
"${cargo_cmd[@]}" build --release --quiet --manifest-path "$repo_root/Cargo.toml" --example bench_io
bench_bin="$repo_root/target/release/examples/bench_io"
if [[ -z "${DBFS_BIN:-}" ]]; then
  "${cargo_cmd[@]}" build --release --quiet --manifest-path "$repo_root/Cargo.toml"
  DBFS_BIN="$repo_root/target/release/dbfs"
fi
mkdir -p "$bench_root/results"

# Starts dbfs on the existing database in $run_dir (no-op with --raw), and
# waits until $run_dir/mnt is a FUSE mount.
mount_dbfs() {
  [[ "$raw" -eq 1 ]] && return 0
  "$DBFS_BIN" mount "$run_dir/db.sqlite" "$run_dir/mnt" >/dev/null 2>>"$run_dir/dbfs.log" &
  dbfs_pid=$!
  for _ in $(seq 300); do
    if [[ "$(mounted_fstype "$run_dir/mnt")" == fuse* ]]; then
      break
    fi
    kill -0 "$dbfs_pid" 2>/dev/null || { cat "$run_dir/dbfs.log" >&2; exit 1; }
    sleep 0.05
  done
  if [[ "$(mounted_fstype "$run_dir/mnt")" != fuse* ]]; then
    echo "mount timed out, or $run_dir/mnt is not a FUSE mount (found: $(mounted_fstype "$run_dir/mnt"))" >&2
    exit 1
  fi
}

# Empty database and mount point in $run_dir.
fresh_target() {
  unmount
  if [[ -n "$(mounted_fstype "$run_dir/mnt")" ]]; then
    echo "refusing: $run_dir/mnt is still mounted; unmount it first" >&2
    exit 1
  fi
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

# Sets target_fstype and storage_fstype for the mount that the timed run measures
# and for the directory that holds the database. Call it while the mount is up.
record_fstypes() {
  target_fstype="$(findmnt -n -o FSTYPE -T "$target_dir")"
  storage_fstype="$(findmnt -n -o FSTYPE -T "$run_dir")"
}

# Runs one workload and prints bench_io's JSON lines. Not run in a subshell:
# the dbfs mount is started and stopped from here, and record_fstypes sets globals.
run_one() {
  local workload=$1 mode
  case "$workload" in
    seqread-cold|randread-cold)
      mode=seq
      [[ $workload == randread-cold ]] && mode=rand
      fresh_target
      "$bench_bin" layout "$target_dir" "$cold_size" >/dev/null
      cold_reset
      record_fstypes
      "$bench_bin" read-only "$target_dir" "$mode" "$cold_size"
      ;;
    tree-cold)
      fresh_target
      "$bench_bin" tree-layout "$target_dir" >/dev/null
      cold_reset
      record_fstypes
      "$bench_bin" tree-stat "$target_dir"
      "$bench_bin" tree-rm "$target_dir"
      ;;
    *)
      fresh_target
      record_fstypes
      "$bench_bin" "$workload" "$target_dir"
      ;;
  esac
}

target_name="dbfs:$DBFS_BIN"
backend="dbfs"
if [[ "$raw" -eq 1 ]]; then
  target_name="raw:$(findmnt -n -o FSTYPE -T "$bench_root")"
  backend="raw"
fi
echo "label=$label target=$target_name repeat=$repeat -> $results" >&2

for workload in $workloads; do
  for i in $(seq "$repeat"); do
    sync
    run_one "$workload" >"$out"
    unmount
    while IFS= read -r line; do
      [[ -n "$line" ]] || continue
      line="${line%\}},\"label\":\"$label\",\"run\":$i,\"backend\":\"$backend\",\"target\":\"$target_dir\",\"target_fstype\":\"$target_fstype\",\"storage_fstype\":\"$storage_fstype\"}"
      echo "$line" | tee -a "$results"
    done <"$out"
  done
done
