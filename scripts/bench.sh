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

while [[ $# -gt 0 ]]; do
  case "$1" in
    --label) label="${2:?}"; shift 2 ;;
    --repeat) repeat="${2:?}"; shift 2 ;;
    --workloads) workloads="${2:?}"; shift 2 ;;
    --raw) raw=1; shift ;;
    --allow-tmpfs) allow_tmpfs=1; shift ;;
    -h|--help) sed -n '2,/^set -euo/{/^set -euo/!p}' "$0"; exit 0 ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done

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

cleanup() {
  set +e
  unmount
  # rm -rf would walk into a mount that is still there, so keep the directory instead.
  if [[ -n "$(mounted_fstype "$run_dir/mnt")" ]]; then
    echo "not removing $run_dir: $run_dir/mnt is still mounted" >&2
  else
    rm -rf "$run_dir"
  fi
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

fresh_target() {
  unmount
  if [[ -n "$(mounted_fstype "$run_dir/mnt")" ]]; then
    echo "refusing: $run_dir/mnt is still mounted; unmount it first" >&2
    exit 1
  fi
  rm -rf "$run_dir"
  mkdir -p "$run_dir/mnt"
  target_dir="$run_dir/mnt"
  [[ "$raw" -eq 1 ]] && return
  "$DBFS_BIN" mount "$run_dir/db.sqlite" "$run_dir/mnt" >/dev/null 2>"$run_dir/dbfs.log" &
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
    fresh_target
    target_fstype="$(findmnt -n -o FSTYPE -T "$target_dir")"
    storage_fstype="$(findmnt -n -o FSTYPE -T "$run_dir")"
    line="$("$bench_bin" "$workload" "$target_dir")"
    unmount
    line="${line%\}},\"label\":\"$label\",\"run\":$i,\"backend\":\"$backend\",\"target\":\"$target_dir\",\"target_fstype\":\"$target_fstype\",\"storage_fstype\":\"$storage_fstype\"}"
    echo "$line" | tee -a "$results"
  done
done
