#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: scripts/bench-fio.sh [OPTIONS] [-- FIO_ARGS...]

Mount dbfs in a temporary directory and benchmark it with fio.

Options:
  --job NAME          fio rw mode to run (default: randwrite)
                      Use "suite" for write, randwrite, read, randread.
  --size SIZE         per-job fio data size (default: 512M)
  --bs SIZE           fio block size (default: 4k)
  --fsync N           fio fsync frequency (default: 1)
  --jobs N            fio numjobs (default: 1)
  --iodepth N         fio iodepth (default: 1)
  --runtime SECONDS   run a time-based benchmark for this long
  --database PATH     sqlite database path to use
  --mountpoint PATH   mountpoint to use
  --keep              keep the database and temp directory after the run
  -h, --help          show this help

Environment overrides:
  DBFS_BIN            dbfs binary to mount (default: target/release/dbfs)
  FIO_SIZE            same as --size
  FIO_BS              same as --bs
  FIO_FSYNC           same as --fsync
  FIO_NUMJOBS         same as --jobs
  FIO_IODEPTH         same as --iodepth
  FIO_RUNTIME         same as --runtime

Examples:
  scripts/bench-fio.sh
  scripts/bench-fio.sh --job suite --size 1G --bs 4k
  scripts/bench-fio.sh --job randwrite -- --group_reporting --output-format=json
EOF
}

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
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

need_command cargo
need_command fio
need_command fusermount3
need_command mountpoint

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

tmp_root="$(mktemp -d "${TMPDIR:-/tmp}/dbfs-fio.XXXXXX")"
if [[ -z "$database" ]]; then
  database="$tmp_root/dbfs.sqlite"
fi
if [[ -z "$mountpoint" ]]; then
  mountpoint="$tmp_root/mnt"
fi

mkdir -p "$mountpoint"
dbfs_pid=""

cleanup() {
  set +e
  if [[ -n "$dbfs_pid" ]]; then
    fusermount3 -z -u "$mountpoint" >/dev/null 2>&1
    wait "$dbfs_pid" >/dev/null 2>&1
  fi
  if [[ "$keep" -eq 0 ]]; then
    rm -rf "$tmp_root"
  else
    echo "kept benchmark files under: $tmp_root"
  fi
}
trap cleanup EXIT INT TERM

echo "mounting dbfs:"
echo "  database:   $database"
echo "  mountpoint: $mountpoint"
"$dbfs_bin" mount "$database" "$mountpoint" &
dbfs_pid="$!"

for _ in {1..300}; do
  if mountpoint -q "$mountpoint"; then
    break
  fi
  if ! kill -0 "$dbfs_pid" >/dev/null 2>&1; then
    echo "dbfs mount process exited before the mount was ready" >&2
    exit 1
  fi
  sleep 0.1
done

if ! mountpoint -q "$mountpoint"; then
  echo "timed out waiting for dbfs mount" >&2
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
