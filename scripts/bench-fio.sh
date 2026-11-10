#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: scripts/bench-fio.sh [OPTIONS] [-- FIO_ARGS...]

Mount dbfs on a fresh directory under .bench/ and benchmark it with fio.
The default location is disk-backed .bench/, never /tmp.

Options:
  --job NAME          fio rw mode to run (default: randwrite)
                      Use "suite" for write, randwrite, read, randread.
  --size SIZE         per-job fio data size (default: 512M)
  --bs SIZE           fio block size (default: 4k)
  --fsync N           fio fsync frequency (default: 1)
  --jobs N            fio numjobs (default: 1)
  --iodepth N         fio iodepth (default: 1)
  --runtime SECONDS   run a time-based benchmark for this long
  --database PATH     sqlite database path (default: under .bench/)
  --mountpoint PATH   mountpoint, must not already be mounted (default: under .bench/)
  --allow-tmpfs       allow the database directory or mountpoint on tmpfs or ramfs.
                      Those are RAM, so results measure memory, not disk.
  --keep              keep the database and temp directory after the run
  -h, --help          show this help

Environment overrides:
  DBFS_BIN            dbfs binary to mount (default: target/release/dbfs)
  DBFS_BENCH_ALLOW_TMPFS=1  same as --allow-tmpfs
  FIO_SIZE            same as --size
  FIO_BS              same as --bs
  FIO_FSYNC           same as --fsync
  FIO_NUMJOBS         same as --jobs
  FIO_IODEPTH         same as --iodepth
  FIO_RUNTIME         same as --runtime

Safety checks: the script refuses to start if the database directory or the
mountpoint is on tmpfs or ramfs (unless --allow-tmpfs), or if the mountpoint is
already mounted. After mounting, it fails unless the mountpoint is a FUSE mount.

Examples:
  scripts/bench-fio.sh
  scripts/bench-fio.sh --job suite --size 1G --bs 4k
  scripts/bench-fio.sh --job randwrite -- --group_reporting --output-format=json
  scripts/bench-fio.sh --database /srv/bench/dbfs.sqlite --mountpoint /srv/bench/mnt
EOF
}

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
bench_root="$repo_root/.bench"
if [[ -n "${DBFS_BIN:-}" ]]; then
  dbfs_bin="$DBFS_BIN"
  build_dbfs=0
else
  dbfs_bin="$repo_root/target/release/dbfs"
  build_dbfs=1
fi
job="randwrite"
size="${FIO_SIZE:-512M}"
bs="${FIO_BS:-4k}"
fsync="${FIO_FSYNC:-1}"
numjobs="${FIO_NUMJOBS:-1}"
iodepth="${FIO_IODEPTH:-1}"
runtime="${FIO_RUNTIME:-}"
database=""
mountpoint=""
allow_tmpfs="${DBFS_BENCH_ALLOW_TMPFS:-0}"
keep=0
fio_extra=()

while [[ $# -gt 0 ]]; do
  case "$1" in
    --job)
      job="${2:?--job requires a value}"
      shift 2
      ;;
    --size)
      size="${2:?--size requires a value}"
      shift 2
      ;;
    --bs)
      bs="${2:?--bs requires a value}"
      shift 2
      ;;
    --fsync)
      fsync="${2:?--fsync requires a value}"
      shift 2
      ;;
    --jobs)
      numjobs="${2:?--jobs requires a value}"
      shift 2
      ;;
    --iodepth)
      iodepth="${2:?--iodepth requires a value}"
      shift 2
      ;;
    --runtime)
      runtime="${2:?--runtime requires a value}"
      shift 2
      ;;
    --database)
      database="${2:?--database requires a value}"
      shift 2
      ;;
    --mountpoint)
      mountpoint="${2:?--mountpoint requires a value}"
      shift 2
      ;;
    --allow-tmpfs)
      allow_tmpfs=1
      shift
      ;;
    --keep)
      keep=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    --)
      shift
      fio_extra=("$@")
      break
      ;;
    *)
      echo "unknown option: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

need_command() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "missing required command: $1" >&2
    exit 1
  fi
}

# Prints the filesystem type of the nearest existing directory at or above $1.
existing_fstype() {
  local path="$1"
  while [[ ! -e "$path" ]]; do
    path="$(dirname "$path")"
  done
  findmnt -n -o FSTYPE -T "$path"
}

# Refuses a directory on tmpfs or ramfs unless --allow-tmpfs was given.
# $1 is the directory, $2 names it in messages.
check_disk_backed() {
  local dir="$1" what="$2" fstype
  fstype="$(existing_fstype "$dir")"
  case "$fstype" in
    tmpfs|ramfs)
      if [[ "$allow_tmpfs" != 1 ]]; then
        echo "refusing: the $what ($dir) is on $fstype, which is RAM, not disk." >&2
        echo "Use a disk-backed directory, or pass --allow-tmpfs to benchmark RAM on purpose." >&2
        exit 1
      fi
      echo "warning: the $what ($dir) is on $fstype; this measures RAM" >&2
      ;;
  esac
}

# Prints the filesystem type mounted exactly at $1, or nothing.
mounted_fstype() {
  findmnt -n -o FSTYPE --mountpoint "$1" 2>/dev/null | tail -n 1 || true
}

need_command cargo
need_command fio
need_command fusermount3
need_command findmnt

if [[ ! -e /dev/fuse ]]; then
  echo "missing /dev/fuse; load/enable FUSE before benchmarking dbfs" >&2
  exit 1
fi

if [[ "$build_dbfs" -eq 1 ]]; then
  echo "building dbfs release binary..."
  cargo build --release --manifest-path "$repo_root/Cargo.toml"
elif [[ ! -x "$dbfs_bin" ]]; then
  echo "DBFS_BIN is not executable: $dbfs_bin" >&2
  exit 1
fi

mkdir -p "$bench_root"
tmp_root="$(mktemp -d "$bench_root/dbfs-fio.XXXXXX")"
if [[ -z "$database" ]]; then
  database="$tmp_root/dbfs.sqlite"
fi
if [[ -z "$mountpoint" ]]; then
  mountpoint="$tmp_root/mnt"
fi

dbfs_pid=""

cleanup() {
  set +e
  if [[ -n "$dbfs_pid" ]]; then
    fusermount3 -z -u "$mountpoint" >/dev/null 2>&1
    wait "$dbfs_pid" >/dev/null 2>&1
  fi
  if [[ "$keep" -eq 0 ]]; then
    # rm -rf would walk into a mount that is still there, so keep the directory instead.
    if [[ -n "$(mounted_fstype "$mountpoint")" ]]; then
      echo "not removing $tmp_root: $mountpoint is still mounted" >&2
    else
      rm -rf "$tmp_root"
    fi
  else
    echo "kept benchmark files under: $tmp_root"
  fi
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

check_disk_backed "$(dirname "$database")" "database directory"
check_disk_backed "$mountpoint" "mountpoint"

if [[ -n "$(mounted_fstype "$mountpoint")" ]]; then
  echo "refusing: $mountpoint is already a mount point ($(mounted_fstype "$mountpoint")); unmount it first" >&2
  exit 1
fi

mkdir -p "$mountpoint"

echo "mounting dbfs:"
echo "  database:   $database (on $(existing_fstype "$(dirname "$database")"))"
echo "  mountpoint: $mountpoint"
"$dbfs_bin" mount "$database" "$mountpoint" &
dbfs_pid="$!"

for _ in {1..300}; do
  if [[ "$(mounted_fstype "$mountpoint")" == fuse* ]]; then
    break
  fi
  if ! kill -0 "$dbfs_pid" >/dev/null 2>&1; then
    echo "dbfs mount process exited before the mount was ready" >&2
    exit 1
  fi
  sleep 0.1
done

mounted="$(mounted_fstype "$mountpoint")"
if [[ "$mounted" != fuse* ]]; then
  echo "timed out waiting for a dbfs FUSE mount at $mountpoint (found: ${mounted:-nothing})" >&2
  exit 1
fi

run_fio() {
  local rw="$1"
  local name="dbfs-$rw"
  local args=(
    --name="$name"
    --directory="$mountpoint"
    --size="$size"
    --bs="$bs"
    --rw="$rw"
    --ioengine=sync
    --iodepth="$iodepth"
    --numjobs="$numjobs"
    --fsync="$fsync"
  )

  if [[ -n "$runtime" ]]; then
    args+=(--time_based --runtime="$runtime")
  fi

  echo
  echo "running fio job: $rw"
  fio "${args[@]}" "${fio_extra[@]}"
}

case "$job" in
  suite)
    run_fio write
    run_fio randwrite
    run_fio read
    run_fio randread
    ;;
  read|write|randread|randwrite|rw|randrw|readwrite|trim|randtrim|trimwrite)
    run_fio "$job"
    ;;
  *)
    echo "unsupported --job value: $job" >&2
    echo "use a fio rw mode such as randwrite, randread, write, read, or suite" >&2
    exit 2
    ;;
esac
