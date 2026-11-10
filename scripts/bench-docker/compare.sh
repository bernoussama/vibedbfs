#!/usr/bin/env bash
# Compares dbfs with ext4, btrfs and libfuse's passthrough_hp, all from one
# privileged container (scripts/bench-docker/Dockerfile) on the same disk.
#
#   scripts/bench-docker/compare.sh [--rounds N] [--fio-rounds N]
#
# The repository is bind-mounted at its host path, so the host-built dbfs and
# bench_io binaries run unchanged and results land in .bench/fscmp2/ (disk, not
# tmpfs). ext4 is the host filesystem under .bench/; btrfs is a loop image on
# it (losetup --direct-io=on). The FUSE configs get these speed-ups:
#
#   pt-*        passthrough_hp: kernel passthrough for reads and writes,
#               multithreaded, clone-fd, splice, FUSE over io_uring, writeback
#               cache, 1-day TTLs, keep_cache, 4 MiB requests
#   pt-*-user   the same with --nopassthrough (data goes through user space)
#   dbfs-*      dbfs defaults, without --kernel-cache
#   dbfs-*-kc   dbfs --kernel-cache (writeback cache, parallel dirops, 1 MiB
#               requests, 1-day TTLs, keep_cache); fuser 0.16 is single-threaded
#
# Each bench_io row records target (the path benchmarked), target_fstype (the
# filesystem mounted there) and storage_fstype (the filesystem holding the data).
# fio results are named <config>-<job>-<round>.json; .bench/fscmp2/fio/targets.tsv
# gives the same three fields for each of them.
#
# Needs Docker with --privileged (loop devices, mounting, FUSE io_uring). The
# host's /sys/module/fuse/parameters/enable_uring is set to Y for the run and
# restored afterwards.
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
rounds=3
fio_rounds=2
while [[ $# -gt 0 ]]; do
  case "$1" in
    --rounds) rounds="$2"; shift 2 ;;
    --fio-rounds) fio_rounds="$2"; shift 2 ;;
    --inside) shift; break ;;
    *) sed -n '2,/^set -euo/{/^set -euo/!p}' "$0"; exit 2 ;;
  esac
done

if [[ "${1:-}" != "inside" ]]; then
  export PATH=$HOME/.cargo/bin:$PATH
  cargo build --release --quiet --manifest-path "$repo/Cargo.toml" --example bench_io
  cargo build --release --quiet --manifest-path "$repo/Cargo.toml"
  docker build -q -t dbfs-bench:dev "$repo/scripts/bench-docker" >/dev/null
  exec docker run --rm --privileged -v "$repo:$repo" -e HOST_UID="$(id -u)" -e HOST_GID="$(id -g)" \
    dbfs-bench:dev "$repo/scripts/bench-docker/compare.sh" --rounds "$rounds" --fio-rounds "$fio_rounds" --inside inside
fi

# ---- inside the container, as root ----
dbfs="$repo/target/release/dbfs"
bench="$repo/target/release/examples/bench_io"
work="$repo/.bench/fscmp2"
workloads="seqwrite randwrite seqread randread metadata bigdir"
configs="ext4 btrfs pt-ext4 pt-ext4-user pt-btrfs pt-btrfs-user dbfs-ext4 dbfs-ext4-kc dbfs-btrfs dbfs-btrfs-kc"
fio_jobs=(
  "randwrite-nofsync|--rw=randwrite --size=256M --bs=4k --fsync=0"
  "randwrite-fsync|--rw=randwrite --size=64M --bs=4k --fsync=1 --time_based --runtime=30"
  "randread-cold|--rw=randread --size=256M --bs=4k --fsync=0"
  "seqwrite|--rw=write --size=512M --bs=128k --fsync=0"
)

# Set as the script goes. cleanup() must work when any of them is still empty.
uring_before=""
loopdev=""
fuse_pid=""
backing=""

# Prints the filesystem type mounted exactly at $1, or nothing.
mounted_fstype() {
  findmnt -n -o FSTYPE --mountpoint "$1" 2>/dev/null | tail -n 1 || true
}

# True while process $1 exists and has not exited. An unreaped zombie counts as exited.
server_running() {
  local state
  state=$(cut -d' ' -f3 "/proc/$1/stat" 2>/dev/null) || return 1
  [[ "$state" != Z ]]
}

cleanup() {
  set +e
  stop_fuse
  umount "$work/btrfs" 2>/dev/null || umount -l "$work/btrfs" 2>/dev/null
  if [[ -n "$loopdev" ]]; then
    losetup -d "$loopdev" 2>/dev/null
  fi
  rm -f "$work/btrfs.img"
  rm -rf "$work/ext4"
  # rm -rf would walk into a mount that is still there, so keep the directory instead.
  if [[ -z "$(mounted_fstype "$work/mnt")" ]]; then
    rm -rf "$work/mnt"
  fi
  if [[ -n "$uring_before" ]]; then
    echo "$uring_before" > /sys/module/fuse/parameters/enable_uring
  fi
  if [[ -n "${HOST_UID:-}" && -n "${HOST_GID:-}" ]]; then
    chown -R "$HOST_UID:$HOST_GID" "$work" 2>/dev/null
  fi
  return 0
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# Unmounts the FUSE config and waits for its server. If the server is still
# running 10 s after the unmount, it is killed, so the script cannot hang.
stop_fuse() {
  [[ -n "$fuse_pid" ]] || return 0
  umount "$work/mnt" 2>/dev/null || umount -l "$work/mnt" 2>/dev/null || true
  for _ in $(seq 100); do
    server_running "$fuse_pid" || break
    sleep 0.1
  done
  if server_running "$fuse_pid"; then
    echo "FUSE server $fuse_pid did not exit 10 s after unmount; killing it" >&2
    kill -KILL "$fuse_pid" 2>/dev/null || true
    umount -l "$work/mnt" 2>/dev/null || true
  fi
  wait "$fuse_pid" 2>/dev/null || true
  fuse_pid=""
}

wait_mounted() {
  for _ in $(seq 200); do
    if [[ "$(mounted_fstype "$work/mnt")" == fuse* ]]; then
      return 0
    fi
    server_running "$fuse_pid" || { echo "FUSE server exited" >&2; exit 1; }
    sleep 0.05
  done
  echo "mount timed out, or $work/mnt is not a FUSE mount (found: $(mounted_fstype "$work/mnt"))" >&2
  exit 1
}

# Prints a fresh, empty directory to benchmark for config $1, and sets $backing
# to the filesystem directory that holds its data.
fresh() {
  local config=$1
  stop_fuse
  case "$config" in *ext4*) backing="$work/ext4" ;; *btrfs*) backing="$work/btrfs" ;; esac
  rm -rf "${backing:?}/t"
  mkdir -p "$backing/t" "$work/mnt"
  if [[ -n "$(mounted_fstype "$work/mnt")" ]]; then
    echo "refusing: $work/mnt is already mounted" >&2
    exit 1
  fi
  sync
  case "$config" in
    ext4|btrfs) echo "$backing/t"; return ;;
    pt-*-user) passthrough_hp --foreground --clone-fd --nopassthrough -o io_uring "$backing/t" "$work/mnt" >/dev/null 2>&1 & ;;
    pt-*) passthrough_hp --foreground --clone-fd -o io_uring "$backing/t" "$work/mnt" >/dev/null 2>&1 & ;;
    dbfs-*-kc) "$dbfs" mount --kernel-cache "$backing/t/db.sqlite" "$work/mnt" >/dev/null 2>&1 & ;;
    dbfs-*) "$dbfs" mount "$backing/t/db.sqlite" "$work/mnt" >/dev/null 2>&1 & ;;
  esac
  fuse_pid=$!
  wait_mounted
  echo "$work/mnt"
}

uring_before=$(cat /sys/module/fuse/parameters/enable_uring)
echo Y > /sys/module/fuse/parameters/enable_uring

mkdir -p "$work"
rm -rf "$work/results" "$work/fio" "$work/ext4"
mkdir -p "$work/results" "$work/fio" "$work/ext4" "$work/btrfs" "$work/mnt"
printf 'file\tconfig\tjob\tround\ttarget\ttarget_fstype\tstorage_fstype\n' >"$work/fio/targets.tsv"
truncate -s 24G "$work/btrfs.img"
mkfs.btrfs -q -f "$work/btrfs.img"
loopdev=$(losetup --direct-io=on -f --show "$work/btrfs.img")
mount "$loopdev" "$work/btrfs"

for round in $(seq "$rounds"); do
  for config in $configs; do
    for workload in $workloads; do
      fresh "$config" >"$work/target"; target=$(cat "$work/target")
      target_fstype=$(findmnt -n -o FSTYPE -T "$target")
      storage_fstype=$(findmnt -n -o FSTYPE -T "$backing/t")
      "$bench" "$workload" "$target" | TARGET="$target" TARGET_FSTYPE="$target_fstype" STORAGE_FSTYPE="$storage_fstype" python3 -c "import json,os,sys; r=json.loads(sys.stdin.read()); r['label']='$config'; r['run']=$round; r['target']=os.environ['TARGET']; r['target_fstype']=os.environ['TARGET_FSTYPE']; r['storage_fstype']=os.environ['STORAGE_FSTYPE']; print(json.dumps(r))" \
        >>"$work/results/$config.jsonl"
    done
    stop_fuse
    echo "round=$round $config bench_io done"
  done
done

for round in $(seq "$fio_rounds"); do
  for config in $configs; do
    for job in "${fio_jobs[@]}"; do
      name=${job%%|*}; read -r -a args <<<"${job#*|}"
      fresh "$config" >"$work/target"; target=$(cat "$work/target")
      target_fstype=$(findmnt -n -o FSTYPE -T "$target")
      storage_fstype=$(findmnt -n -o FSTYPE -T "$backing/t")
      printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$config-$name-$round.json" "$config" "$name" "$round" \
        "$target" "$target_fstype" "$storage_fstype" >>"$work/fio/targets.tsv"
      common=(--name="$name" --directory="$target" --ioengine=sync --iodepth=1 --numjobs=1 "${args[@]}")
      # Lay the file out first, then drop every cache (the backing filesystem's
      # included, which fio's own invalidate does not reach under FUSE).
      [[ $name == randread-cold ]] && fio "${common[@]}" --create_only=1 >/dev/null
      sync; echo 3 > /proc/sys/vm/drop_caches
      fio --name="$name" --directory="$target" --ioengine=sync --iodepth=1 --numjobs=1 "${args[@]}" \
        --output-format=json --output="$work/fio/$config-$name-$round.json" >/dev/null
    done
    stop_fuse
    echo "round=$round $config fio done"
  done
done
echo ALL DONE
