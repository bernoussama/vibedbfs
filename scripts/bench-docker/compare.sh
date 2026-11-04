#!/usr/bin/env bash
# Compares dbfs with ext4, btrfs and libfuse's passthrough_hp, all from one
# privileged container (scripts/bench-docker/Dockerfile) on the same disk.
#
#   scripts/bench-docker/compare.sh [--rounds N] [--fio-rounds N]
#
# The repository is bind-mounted at its host path, so the host-built dbfs and
# bench_io binaries run unchanged and results land in .bench/fscmp2/ (disk, not
# tmpfs). ext4 is the host filesystem under .bench/; btrfs is a loop image on
# it (losetup --direct-io=on). Every FUSE mount gets every speed-up available:
#
#   pt-*        passthrough_hp: kernel passthrough for reads and writes,
#               multithreaded, clone-fd, splice, FUSE over io_uring, writeback
#               cache, 1-day TTLs, keep_cache, 4 MiB requests
#   pt-*-user   the same with --nopassthrough (data goes through user space)
#   dbfs-*      dbfs defaults
#   dbfs-*-kc   dbfs --kernel-cache (writeback cache, parallel dirops, 1 MiB
#               requests, 1-day TTLs, keep_cache); fuser 0.16 is single-threaded
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
    *) sed -n 2,24p "$0"; exit 2 ;;
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

uring_before=$(cat /sys/module/fuse/parameters/enable_uring)
echo Y > /sys/module/fuse/parameters/enable_uring

mkdir -p "$work"
rm -rf "$work/results" "$work/fio" "$work/ext4"
mkdir -p "$work/results" "$work/fio" "$work/ext4" "$work/btrfs" "$work/mnt"
truncate -s 24G "$work/btrfs.img"
mkfs.btrfs -q -f "$work/btrfs.img"
loopdev=$(losetup --direct-io=on -f --show "$work/btrfs.img")
mount "$loopdev" "$work/btrfs"

fuse_pid=""
cleanup() {
  stop_fuse || true
  umount "$work/btrfs" 2>/dev/null || umount -l "$work/btrfs" || true
  losetup -d "$loopdev" 2>/dev/null || true
  rm -f "$work/btrfs.img"
  rm -rf "$work/ext4" "$work/mnt"
  echo "$uring_before" > /sys/module/fuse/parameters/enable_uring
  chown -R "$HOST_UID:$HOST_GID" "$work"
}
trap cleanup EXIT

stop_fuse() {
  if [[ -n "$fuse_pid" ]]; then
    umount "$work/mnt" 2>/dev/null || umount -l "$work/mnt" 2>/dev/null || true
    wait "$fuse_pid" 2>/dev/null || true
    fuse_pid=""
  fi
}

wait_mounted() {
  for _ in $(seq 200); do
    mountpoint -q "$work/mnt" && return
    kill -0 "$fuse_pid" 2>/dev/null || { echo "FUSE server exited" >&2; exit 1; }
    sleep 0.05
  done
  echo "mount timed out" >&2; exit 1
}

# Prints a fresh, empty directory to benchmark for config $1.
fresh() {
  local config=$1 backing
  stop_fuse
  case "$config" in *ext4*) backing="$work/ext4" ;; *btrfs*) backing="$work/btrfs" ;; esac
  rm -rf "${backing:?}/t"
  mkdir -p "$backing/t" "$work/mnt"
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

for round in $(seq "$rounds"); do
  for config in $configs; do
    for workload in $workloads; do
      fresh "$config" >"$work/target"; target=$(cat "$work/target")
      "$bench" "$workload" "$target" | python3 -c "import json,sys; r=json.loads(sys.stdin.read()); r['label']='$config'; r['run']=$round; print(json.dumps(r))" \
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
