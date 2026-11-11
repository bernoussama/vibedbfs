#!/usr/bin/env bash
# Compares dbfs with ext4, btrfs and libfuse's passthrough_hp, all from one
# privileged container (scripts/bench-docker/Dockerfile) on the same disk.
#
#   scripts/bench-docker/compare.sh [OPTIONS]
#
#   --quick             configs ext4 pt-ext4-user dbfs-ext4 dbfs-ext4-kc, 1 bench_io
#                       round and 1 fio round. Explicit options still win.
#   --rounds N          bench_io rounds (default 3)
#   --fio-rounds N      fio rounds (default 2)
#   --configs "LIST"    configs to run, space separated (default: all ten)
#   --workloads "LIST"  workloads by name (default: all). A name selects the
#                       bench_io workload and the fio job with the same name.
#   --dbfs-args "ARGS"  extra arguments for every dbfs mount. They go after the
#                       config's own flags and before the database path, e.g.
#                       "--write-buffer 8M" for an A/B test of one flag.
#   --tag NAME          results go to .bench/fscmp2/NAME/ (default: a timestamp).
#                       The directory must not exist yet.
#   -h, --help          show the options
#
# The repository is bind-mounted at its host path, so the host-built dbfs and
# bench_io binaries run unchanged and results land in .bench/fscmp2/<tag>/ (disk,
# not tmpfs). ext4 is the host filesystem under .bench/; btrfs is a loop image on
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
# Cold reads (seqread-cold, randread-cold, tree-cold, and the randread-cold fio
# job) lay the data out, sync, drop the page cache of the host
# (echo 3 > /proc/sys/vm/drop_caches), and remount the FUSE filesystem before
# the timed part. Native configs only drop the caches.
#
# Each bench_io row records target (the path benchmarked), target_fstype (the
# filesystem mounted there) and storage_fstype (the filesystem holding the data).
# fio/targets.tsv gives the same three fields for each fio file.
#
# Results: .bench/fscmp2/<tag>/
#   run.txt                            command lines, git rev, dbfs version, kernel
#   results/<config>.jsonl             bench_io records, with label, run and the three fields
#   raw/<config>-<workload>-<round>.jsonl   bench_io output as printed
#   fio/<config>/<job>-<round>.json    fio --output-format=json, unmodified
#   fio/targets.tsv                    target, target_fstype, storage_fstype of each fio file
#   logs/<config>.log                  stderr of each FUSE server
#   summary.txt                        scripts/bench-docker/summarize.py output
#
# Needs Docker with --privileged (loop devices, mounting, FUSE io_uring). The
# host's /sys/module/fuse/parameters/enable_uring is set to Y for the run and
# restored afterwards. Everything under .bench/ is chowned back to the host user
# when the container exits, failures included.
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
script="$repo/scripts/bench-docker/compare.sh"

all_configs=(ext4 btrfs pt-ext4 pt-ext4-user pt-btrfs pt-btrfs-user dbfs-ext4 dbfs-ext4-kc dbfs-btrfs dbfs-btrfs-kc)
quick_configs=(ext4 pt-ext4-user dbfs-ext4 dbfs-ext4-kc)
bench_workloads=(seqwrite randwrite seqread randread metadata bigdir seqwrite-long seqread-cold randread-cold smallfiles tree-cold)
# "name|fio arguments". All jobs use the sync engine with iodepth 1.
fio_jobs=(
  "randwrite-nofsync|--rw=randwrite --size=256M --bs=4k --fsync=0 --numjobs=1"
  "randwrite-fsync|--rw=randwrite --size=64M --bs=4k --fsync=1 --numjobs=1 --time_based --runtime=30"
  "randread-cold|--rw=randread --size=256M --bs=4k --fsync=0 --numjobs=1"
  "seqwrite|--rw=write --size=512M --bs=128k --fsync=0 --numjobs=1"
  "randwrite-4jobs|--rw=randwrite --size=64M --bs=4k --fsync=0 --numjobs=4 --group_reporting"
)
fio_names=()
for job in "${fio_jobs[@]}"; do fio_names+=("${job%%|*}"); done
cold_size=256M

die() { echo "compare.sh: $*" >&2; exit 2; }

contains() { # needle item...
  local needle=$1 item
  shift
  for item in "$@"; do [[ $item == "$needle" ]] && return 0; done
  return 1
}

usage() {
  cat <<EOF
Usage: scripts/bench-docker/compare.sh [OPTIONS]

  --quick             configs: ${quick_configs[*]}; 1 bench_io round, 1 fio round
  --rounds N          bench_io rounds (default 3)
  --fio-rounds N      fio rounds (default 2)
  --configs "LIST"    configs to run (default: ${all_configs[*]})
  --workloads "LIST"  workloads by name (default: all). A name selects the bench_io
                      workload and the fio job with that name.
                        bench_io: ${bench_workloads[*]}
                        fio:      ${fio_names[*]}
  --dbfs-args "ARGS"  extra arguments for every dbfs mount, placed after the config's
                      own flags and before the database path
  --tag NAME          results go to .bench/fscmp2/NAME/ (default: timestamp); the
                      directory must not exist yet
  -h, --help          show this help
EOF
}

parse_args() {
  quick=0 inside=0 rounds="" fio_rounds="" configs_arg="" workloads_arg="" dbfs_args="" tag=""
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --quick) quick=1; shift ;;
      --rounds) rounds="${2:?--rounds needs a value}"; shift 2 ;;
      --fio-rounds) fio_rounds="${2:?--fio-rounds needs a value}"; shift 2 ;;
      --configs) configs_arg="${2?--configs needs a value}"; shift 2 ;;
      --workloads) workloads_arg="${2?--workloads needs a value}"; shift 2 ;;
      --dbfs-args) dbfs_args="${2?--dbfs-args needs a value}"; shift 2 ;;
      --tag) tag="${2:?--tag needs a value}"; shift 2 ;;
      --inside) inside=1; shift ;;
      -h|--help) usage; exit 0 ;;
      *) usage >&2; die "unknown option: $1" ;;
    esac
  done
}

# Fills configs, bench_sel and fio_sel from the options, and checks them.
resolve_options() {
  local c w known requested=()
  if [[ -z $rounds ]]; then rounds=$((quick ? 1 : 3)); fi
  if [[ -z $fio_rounds ]]; then fio_rounds=$((quick ? 1 : 2)); fi
  [[ $rounds =~ ^[1-9][0-9]*$ ]] || die "--rounds must be a positive integer"
  [[ $fio_rounds =~ ^[1-9][0-9]*$ ]] || die "--fio-rounds must be a positive integer"

  configs=()
  if [[ -n $configs_arg ]]; then
    read -r -a configs <<<"$configs_arg" || true
  elif (( quick )); then
    configs=("${quick_configs[@]}")
  else
    configs=("${all_configs[@]}")
  fi
  (( ${#configs[@]} > 0 )) || die "no configs selected"
  for c in "${configs[@]}"; do
    contains "$c" "${all_configs[@]}" || die "unknown config: $c (valid: ${all_configs[*]})"
  done

  bench_sel=() fio_sel=()
  if [[ -z $workloads_arg ]]; then
    bench_sel=("${bench_workloads[@]}")
    fio_sel=("${fio_names[@]}")
  else
    read -r -a requested <<<"$workloads_arg" || true
    for w in "${requested[@]}"; do
      known=0
      if contains "$w" "${bench_workloads[@]}"; then bench_sel+=("$w"); known=1; fi
      if contains "$w" "${fio_names[@]}"; then fio_sel+=("$w"); known=1; fi
      (( known )) || die "unknown workload: $w (bench_io: ${bench_workloads[*]}; fio: ${fio_names[*]})"
    done
  fi

  if [[ -z $tag ]]; then tag=$(date +%Y%m%d-%H%M%S); fi
  [[ $tag =~ ^[A-Za-z0-9][A-Za-z0-9._-]*$ && $tag != scratch ]] ||
    die "--tag must start with a letter or digit and use only letters, digits, . _ - (and not be 'scratch')"
}

# Records the command lines, the git revision, dbfs and the kernel next to the
# results. Runs on the host before the container starts.
write_run_info() { # tagdir dbfs_bin docker-command...
  local tagdir=$1 dbfs_bin=$2 rev dirty="" version
  shift 2
  rev=$(git -C "$repo" rev-parse --short HEAD 2>/dev/null || echo unknown)
  if [[ -n $(git -C "$repo" status --porcelain 2>/dev/null) ]]; then
    dirty=" (uncommitted changes, saved in worktree.patch)"
    git -C "$repo" diff HEAD --binary >"$tagdir/worktree.patch" 2>/dev/null || true
  fi
  if ! version=$("$dbfs_bin" --version 2>/dev/null) || [[ -z $version ]]; then
    version="no --version flag; sha256 $(sha256sum "$dbfs_bin" | cut -d' ' -f1)"
  fi
  {
    printf 'command:    %s\n' "$(printf '%q ' "$0" "${original_args[@]}")"
    printf 'container:  %s\n' "$(printf '%q ' "$@")"
    printf 'git:        %s%s\n' "$rev" "$dirty"
    printf 'dbfs:       %s (%s)\n' "$version" "$dbfs_bin"
    printf 'kernel:     %s\n' "$(uname -r)"
    printf 'started:    %s\n' "$(date -Is)"
    printf 'tag:        %s\n' "$tag"
    printf 'rounds:     %s\n' "$rounds"
    printf 'fio-rounds: %s\n' "$fio_rounds"
    printf 'configs:    %s\n' "${configs[*]}"
    printf 'bench_io:   %s\n' "${bench_sel[*]}"
    printf 'fio:        %s\n' "${fio_sel[*]}"
    printf 'dbfs-args:  %s\n' "$dbfs_args"
  } >"$tagdir/run.txt"
}

host_main() {
  local tagdir="$repo/.bench/fscmp2/$tag" dbfs_bin="$repo/target/release/dbfs"
  [[ ! -e $tagdir ]] || die "$tagdir already exists; choose another --tag"
  export PATH="$HOME/.cargo/bin:$PATH"
  cargo build --release --quiet --manifest-path "$repo/Cargo.toml" --example bench_io
  cargo build --release --quiet --manifest-path "$repo/Cargo.toml"
  docker build -q -t dbfs-bench:dev "$repo/scripts/bench-docker" >/dev/null
  mkdir -p "$tagdir"
  local docker_cmd=(docker run --rm --privileged -v "$repo:$repo"
    -e HOST_UID="$(id -u)" -e HOST_GID="$(id -g)" dbfs-bench:dev
    "$script" --inside --tag "$tag" --rounds "$rounds" --fio-rounds "$fio_rounds"
    --configs "${configs[*]}" --workloads "$workloads_arg" --dbfs-args "$dbfs_args")
  write_run_info "$tagdir" "$dbfs_bin" "${docker_cmd[@]}"
  exec "${docker_cmd[@]}"
}

# ---- inside the container, as root ----

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

# Unmounts the FUSE config and waits for its server. If the server is still
# running 10 s after the unmount, it is killed, so the script cannot hang.
stop_fuse() {
  if [[ -n $fuse_pid ]]; then
    umount "$scratch/mnt" 2>/dev/null || umount -l "$scratch/mnt" 2>/dev/null || true
    local _ status=0
    for _ in $(seq 100); do
      server_running "$fuse_pid" || break
      sleep 0.1
    done
    if server_running "$fuse_pid"; then
      echo "FUSE server $fuse_pid did not exit 10 s after unmount; killing it" >&2
      kill -KILL "$fuse_pid" 2>/dev/null || true
      umount -l "$scratch/mnt" 2>/dev/null || true
    fi
    wait "$fuse_pid" 2>/dev/null || status=$?
    if (( status != 0 )); then echo "warning: $fuse_config exited with status $status" >&2; fi
    fuse_pid=""
  fi
}

# Returns once $scratch/mnt is a FUSE mount; exits if the server dies or the mount times out.
wait_mounted() {
  local _
  for _ in $(seq 200); do
    if [[ "$(mounted_fstype "$scratch/mnt")" == fuse* ]]; then
      return 0
    fi
    server_running "$fuse_pid" || { echo "FUSE server exited; see $tagdir/logs/$fuse_config.log" >&2; exit 1; }
    sleep 0.05
  done
  echo "mount timed out, or $scratch/mnt is not a FUSE mount (found: $(mounted_fstype "$scratch/mnt"))" >&2
  exit 1
}

backing_for() {
  case "$1" in *btrfs*) echo "$scratch/btrfs" ;; *) echo "$scratch/ext4" ;; esac
}

# Makes config $1 usable without touching its data: sets $target to the
# directory to benchmark, and mounts the FUSE server for FUSE configs.
mount_target() {
  local config=$1 backing log
  backing=$(backing_for "$config")
  target="$backing/t"
  log="$tagdir/logs/$config.log"
  fuse_config=$config
  case "$config" in
    ext4|btrfs) return 0 ;;
    pt-*-user) passthrough_hp --foreground --clone-fd --nopassthrough -o io_uring "$target" "$scratch/mnt" >>"$log" 2>&1 & ;;
    pt-*) passthrough_hp --foreground --clone-fd -o io_uring "$target" "$scratch/mnt" >>"$log" 2>&1 & ;;
    dbfs-*-kc) "$dbfs" mount --kernel-cache "${dbfs_extra[@]}" "$target/db.sqlite" "$scratch/mnt" >>"$log" 2>&1 & ;;
    dbfs-*) "$dbfs" mount "${dbfs_extra[@]}" "$target/db.sqlite" "$scratch/mnt" >>"$log" 2>&1 & ;;
  esac
  fuse_pid=$!
  wait_mounted
  target="$scratch/mnt"
}

# Sets $target to a new, empty benchmark directory for config $1.
fresh() {
  local backing
  backing=$(backing_for "$1")
  stop_fuse
  if [[ -n "$(mounted_fstype "$scratch/mnt")" ]]; then
    echo "refusing: $scratch/mnt is already mounted" >&2
    exit 1
  fi
  rm -rf "${backing:?}/t"
  mkdir -p "$backing/t" "$scratch/mnt"
  sync
  mount_target "$1"
}

# Keeps the data of config $1 but makes the next reads come from the device:
# flushes, drops the page caches and remounts a FUSE filesystem.
cold_reset() {
  stop_fuse
  sync
  echo 3 > /proc/sys/vm/drop_caches
  mount_target "$1"
}

# Sets target_fstype and storage_fstype for $target and for the directory that
# holds its data (config $1). Call it while the timed mount is up.
record_fstypes() {
  target_fstype=$(findmnt -n -o FSTYPE -T "$target")
  storage_fstype=$(findmnt -n -o FSTYPE -T "$(backing_for "$1")/t")
}

# Adds "label", "run", "target", "target_fstype" and "storage_fstype" to every
# JSON line on stdin. Args: config round target target_fstype storage_fstype.
annotate() {
  python3 -c '
import json, sys
label, run = sys.argv[1], int(sys.argv[2])
target, target_fstype, storage_fstype = sys.argv[3:6]
for line in sys.stdin:
    if line.strip():
        record = json.loads(line)
        record["label"] = label
        record["run"] = run
        record["target"] = target
        record["target_fstype"] = target_fstype
        record["storage_fstype"] = storage_fstype
        print(json.dumps(record, separators=(",", ":")))
' "$@"
}

run_bench() { # config workload round
  local config=$1 workload=$2 round=$3 mode
  local out="$tagdir/results/$config.jsonl" raw="$tagdir/raw/$config-$workload-$round.jsonl"
  case "$workload" in
    seqread-cold|randread-cold)
      mode=seq
      [[ $workload == randread-cold ]] && mode=rand
      fresh "$config"
      "$bench" layout "$target" "$cold_size" >>"$raw"
      cold_reset "$config"
      record_fstypes "$config"
      "$bench" read-only "$target" "$mode" "$cold_size" | tee -a "$raw" \
        | annotate "$config" "$round" "$target" "$target_fstype" "$storage_fstype" >>"$out"
      ;;
    tree-cold)
      fresh "$config"
      "$bench" tree-layout "$target" >>"$raw"
      cold_reset "$config"
      record_fstypes "$config"
      "$bench" tree-stat "$target" | tee -a "$raw" \
        | annotate "$config" "$round" "$target" "$target_fstype" "$storage_fstype" >>"$out"
      "$bench" tree-rm "$target" | tee -a "$raw" \
        | annotate "$config" "$round" "$target" "$target_fstype" "$storage_fstype" >>"$out"
      ;;
    *)
      fresh "$config"
      record_fstypes "$config"
      "$bench" "$workload" "$target" | tee -a "$raw" \
        | annotate "$config" "$round" "$target" "$target_fstype" "$storage_fstype" >>"$out"
      ;;
  esac
  stop_fuse
}

fio_args() { # job name -> its fio arguments
  local job
  for job in "${fio_jobs[@]}"; do
    if [[ ${job%%|*} == "$1" ]]; then printf '%s\n' "${job#*|}"; return 0; fi
  done
  return 1
}

run_fio() { # config job round
  local config=$1 job=$2 round=$3 entry args=() outdir="$tagdir/fio/$1"
  entry=$(fio_args "$job")
  read -r -a args <<<"$entry" || true
  mkdir -p "$outdir"
  fresh "$config"
  local common=(--name="$job" --directory="$target" --ioengine=sync --iodepth=1)
  if [[ $job == randread-cold ]]; then
    # Lay the file out, then cold-reset (the randread data must not be cached).
    fio "${common[@]}" "${args[@]}" --create_only=1 >/dev/null
    cold_reset "$config"
  else
    sync
    echo 3 > /proc/sys/vm/drop_caches
  fi
  record_fstypes "$config"
  printf '%s/%s-%s.json\t%s\t%s\t%s\t%s\t%s\t%s\n' "$config" "$job" "$round" \
    "$config" "$job" "$round" "$target" "$target_fstype" "$storage_fstype" >>"$tagdir/fio/targets.tsv"
  fio "${common[@]}" "${args[@]}" --output-format=json --output="$outdir/$job-$round.json" >/dev/null
  stop_fuse
}

setup_btrfs() {
  truncate -s 24G "$scratch/btrfs.img"
  mkfs.btrfs -q -f "$scratch/btrfs.img"
  loopdev=$(losetup --direct-io=on -f --show "$scratch/btrfs.img")
  mkdir -p "$scratch/btrfs"
  mount "$loopdev" "$scratch/btrfs"
  btrfs_mounted=1
}

cleanup() {
  local rc=$?
  trap - EXIT
  set +e
  stop_fuse
  if (( btrfs_mounted )); then
    umount "$scratch/btrfs" 2>/dev/null || umount -l "$scratch/btrfs" 2>/dev/null
    if mountpoint -q "$scratch/btrfs"; then
      echo "warning: $scratch/btrfs is still mounted; keeping $scratch" >&2
      keep_scratch=1
    else
      btrfs_mounted=0
    fi
  fi
  if [[ -n $loopdev ]] && (( ! btrfs_mounted )); then losetup -d "$loopdev" 2>/dev/null; fi
  # rm -rf would walk into a mount that is still there, so keep the directory instead.
  if [[ -n "$(mounted_fstype "$scratch/mnt")" ]]; then
    echo "warning: $scratch/mnt is still mounted; keeping $scratch" >&2
    keep_scratch=1
  fi
  if (( ! keep_scratch )); then rm -rf "${scratch:?}"; fi
  if [[ -n $uring_before ]]; then echo "$uring_before" > /sys/module/fuse/parameters/enable_uring; fi
  # The container wrote everything as root. Give it back to the host user, even
  # after a failure.
  if [[ -n ${HOST_UID:-} && -n ${HOST_GID:-} ]]; then chown -R "$HOST_UID:$HOST_GID" "$repo/.bench"; fi
  exit "$rc"
}

inside_main() {
  dbfs="$repo/target/release/dbfs"
  bench="$repo/target/release/examples/bench_io"
  tagdir="$repo/.bench/fscmp2/$tag"
  scratch="$repo/.bench/fscmp2/scratch"
  read -r -a dbfs_extra <<<"$dbfs_args" || true
  fuse_pid="" fuse_config="" target="" target_fstype="" storage_fstype="" loopdev=""
  btrfs_mounted=0 keep_scratch=0 uring_before=""
  trap cleanup EXIT
  trap 'exit 130' INT
  trap 'exit 143' TERM

  uring_before=$(cat /sys/module/fuse/parameters/enable_uring)
  echo Y > /sys/module/fuse/parameters/enable_uring

  local m config
  for m in mnt btrfs; do
    if mountpoint -q "$scratch/$m" 2>/dev/null; then umount -l "$scratch/$m"; fi
  done
  rm -rf "${scratch:?}"
  mkdir -p "$scratch/ext4" "$scratch/mnt" "$tagdir/results" "$tagdir/raw" "$tagdir/fio" "$tagdir/logs"
  printf 'file\tconfig\tjob\tround\ttarget\ttarget_fstype\tstorage_fstype\n' >"$tagdir/fio/targets.tsv"
  for config in "${configs[@]}"; do
    if [[ $config == *btrfs* ]]; then setup_btrfs; break; fi
  done

  local round job workload
  for round in $(seq "$rounds"); do
    for config in "${configs[@]}"; do
      for workload in "${bench_sel[@]}"; do
        run_bench "$config" "$workload" "$round"
      done
      echo "round=$round $config bench_io done"
    done
  done
  for round in $(seq "$fio_rounds"); do
    for config in "${configs[@]}"; do
      for job in "${fio_sel[@]}"; do
        run_fio "$config" "$job" "$round"
      done
      echo "fio round=$round $config done"
    done
  done

  if python3 "$repo/scripts/bench-docker/summarize.py" "$tagdir" >"$tagdir/summary.txt"; then
    cat "$tagdir/summary.txt"
  else
    echo "summarize.py failed; the raw results are in $tagdir" >&2
  fi
  echo ALL DONE
}

main() {
  original_args=("$@")
  parse_args "$@"
  resolve_options
  if (( inside )); then inside_main; else host_main; fi
}

if [[ ${BASH_SOURCE[0]} == "$0" ]]; then
  main "$@"
fi
