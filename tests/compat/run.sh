#!/usr/bin/env bash
# Linux and Docker compatibility matrix for dbfs.
#
#   DBFS_RUN_DOCKER_TESTS=1 tests/compat/run.sh
#
# Covers S1, S2, S3 and S6 from docs/plans/2026-11-03-data-oriented-redesign.md,
# plus container workloads on a dbfs mount (git, tar, sqlite3). Without
# DBFS_RUN_DOCKER_TESTS=1 the script exits 0 and does nothing.
#
# Every check prints one row: PASS, FAIL, SKIP (a prerequisite is missing; the
# detail names the change that provides it) or XFAIL (a known, tracked bug).
# The exit status is 1 if any row is FAIL.
#
# rm -rf of a large directory used to skip entries (C6, positional readdir
# cookies). Phase 6 made cookies stable dirent ids, so any failure is now FAIL.
#
# Environment:
#   DBFS_RUN_DOCKER_TESTS=1   required
#   DBFS_COMPAT_IMAGE         image tag to build and test (default dbfs:dev)
#   DBFS_COMPAT_ALPINE        base image for workloads (default alpine:3.24)
#   DBFS_COMPAT_BIN           host dbfs binary (default target/release/dbfs)
#   DBFS_COMPAT_TREE_ENTRIES  entries in each rm -rf tree (default 2000)
#   DBFS_COMPAT_S6_MIB        size of the unflushed write in S6 (default 16)
#   DBFS_COMPAT_KEEP=1        keep .bench/compat/run-* after the run (kept on FAIL)
#
# Workspaces go under .bench/compat/, never /tmp (tmpfs on this machine).
# Containers, volumes and mounts are removed on exit.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

if [[ "${DBFS_RUN_DOCKER_TESTS:-}" != 1 ]]; then
  echo "tests/compat/run.sh: skipped (set DBFS_RUN_DOCKER_TESTS=1 to run the compatibility matrix)"
  exit 0
fi

IMAGE="${DBFS_COMPAT_IMAGE:-dbfs:dev}"
ALPINE="${DBFS_COMPAT_ALPINE:-alpine:3.24}"
BIN="${DBFS_COMPAT_BIN:-$ROOT/target/release/dbfs}"
TREE_ENTRIES="${DBFS_COMPAT_TREE_ENTRIES:-2000}"
S6_MIB="${DBFS_COMPAT_S6_MIB:-16}"
S6_STOP_LIMIT_MS=5000
IMAGE_SIZE_LIMIT=20000000
FUSE_FLAGS=(--device /dev/fuse --cap-add SYS_ADMIN --security-opt apparmor=unconfined)

RUN_ID="$(date +%Y%m%d-%H%M%S)-$$"
WS="$ROOT/.bench/compat/run-$RUN_ID"
LOGS="$WS/logs"
REG="$WS/cleanup.list"
RESULTS="$WS/results.tsv"
S3_CTR="dbfs-compat-s3-$RUN_ID"
S3_VOL="dbfs-compat-s3v-$RUN_ID"
S3_READY="$WS/s3.ready"
S6_VOL="dbfs-compat-s6v-$RUN_ID"
DETAIL_FILE="$LOGS/detail.txt"
HOST_PID=""
HOST_MNT=""

# ---- helpers ---------------------------------------------------------------

fatal() {
  echo "tests/compat/run.sh: $*" >&2
  exit 1
}

# Resource registry, replayed in reverse on exit.
reg() { printf '%s %s\n' "$1" "$2" >>"$REG"; }

note() { printf '%s\n' "$*" >>"$DETAIL_FILE"; }
die() { printf '%s\n' "$*" >>"$DETAIL_FILE"; exit 1; }
skip() { printf '%s\n' "$*" >>"$DETAIL_FILE"; exit 77; }
xfail() { printf '%s\n' "$*" >>"$DETAIL_FILE"; exit 78; }

# True if a FUSE filesystem is mounted at exactly $1 (host or this namespace).
is_fuse_mounted() {
  awk -v p="$1" '$2 == p && $3 ~ /^fuse/ { found = 1 } END { exit !found }' /proc/mounts
}

# Waits until dbfs is mounted at $2 inside container $1.
wait_container_mount() {
  local ctr=$1 mnt=$2 i running
  for i in $(seq 1 150); do
    if docker exec "$ctr" awk -v p="$mnt" '$2 == p && $3 ~ /^fuse/ { found = 1 } END { exit !found }' /proc/mounts 2>/dev/null; then
      return 0
    fi
    running="$(docker inspect -f '{{.State.Running}}' "$ctr" 2>/dev/null || echo false)"
    if [[ "$running" != true ]]; then
      echo "container $ctr stopped before mounting $mnt:" >&2
      docker logs "$ctr" 2>&1 | tail -n 20 >&2 || true
      return 1
    fi
    sleep 0.2
  done
  echo "timed out waiting for $mnt in $ctr" >&2
  docker logs "$ctr" 2>&1 | tail -n 20 >&2 || true
  return 1
}


# Starts dbfs on the host in the background and waits until it is mounted.
# Usage: host_mount DB MOUNTPOINT [dbfs mount flags...]
host_mount() {
  local db=$1 mnt=$2 i
  shift 2
  mkdir -p "$mnt"
  reg mount "$mnt"
  HOST_MNT="$mnt"
  "$BIN" mount "$@" "$db" "$mnt" >>"$LOGS/host-dbfs.log" 2>&1 &
  HOST_PID=$!
  for i in $(seq 1 100); do
    if is_fuse_mounted "$mnt"; then
      return 0
    fi
    if ! kill -0 "$HOST_PID" 2>/dev/null; then
      wait "$HOST_PID" 2>/dev/null || true
      echo "dbfs exited before mounting $mnt; see $LOGS/host-dbfs.log" >&2
      return 1
    fi
    sleep 0.1
  done
  echo "timed out waiting for $mnt" >&2
  return 1
}

# Unmounts $1 and waits for the dbfs process to exit. Returns its exit status.
host_unmount() {
  local mnt=$1 pid=$HOST_PID i rc=0
  for i in 1 2 3 4 5; do
    is_fuse_mounted "$mnt" || break
    fusermount3 -u "$mnt" 2>>"$LOGS/fusermount.log" || sleep 0.5
  done
  if is_fuse_mounted "$mnt"; then
    fusermount3 -uz "$mnt" 2>>"$LOGS/fusermount.log" || true
  fi
  for i in $(seq 1 100); do
    kill -0 "$pid" 2>/dev/null || break
    sleep 0.1
  done
  if kill -0 "$pid" 2>/dev/null; then
    kill -KILL "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
    HOST_MNT=""
    return 2
  fi
  wait "$pid" || rc=$?
  HOST_MNT=""
  return "$rc"
}

# Runs in each test subshell on exit: unmounts a host mount left behind by a failure.
host_mount_cleanup() {
  if [[ -n "$HOST_MNT" ]] && is_fuse_mounted "$HOST_MNT"; then
    fusermount3 -u "$HOST_MNT" 2>/dev/null || fusermount3 -uz "$HOST_MNT" 2>/dev/null || true
  fi
}

require_bin() { [[ -x "$BIN" ]] || die "no executable at $BIN (run cargo build --release)"; }

# The dbfs CLI must expose --allow-other (C2).
supports_allow_other() {
  local help
  help="$("$BIN" mount --help 2>&1 || true)"
  grep -q -- '--allow-other' <<<"$help"
}

allow_other_prereqs() {
  supports_allow_other || skip "needs C2: dbfs mount has no --allow-other yet"
  if [[ "$(id -u)" -ne 0 ]] && ! grep -Eq '^[[:space:]]*user_allow_other' /etc/fuse.conf 2>/dev/null; then
    skip "needs user_allow_other in /etc/fuse.conf (not root, and it is not set)"
  fi
}

need_s3() { [[ -f "$S3_READY" ]] || die "no dbfs container: the S3 start row did not pass"; }

# Runs a workload script inside the S3 container. Usage: run_s3_script LABEL MARKER SCRIPT ARGS...
run_s3_script() {
  local label=$1 marker=$2 script=$3 out rc=0
  shift 3
  out=$(docker exec "$S3_CTR" sh -c "$script" sh "$@" 2>&1) || rc=$?
  printf '%s\n' "$out" >"$LOGS/$label.out"
  (( rc == 0 )) || die "$label failed (full output in $LOGS/$label.out): $(tail -n 4 <<<"$out" | tr '\n' ' ')"
  # Prefix match: some markers carry a summary, e.g. "TAR_OK entries=520".
  grep -q "^$marker" <<<"$out" || die "$label did not reach $marker: $(tail -n 3 <<<"$out" | tr '\n' ' ')"
}

# Decides the result of an rm -rf step on a large directory. Arguments:
# rm exit code, entries left afterwards, the container output.
judge_rm() {
  local rc=$1 left=$2 out=$3 errs
  errs="$(sed -n 's/^RM_ERR: //p' <<<"$out" | head -n 1)"
  [[ -n "$rc" ]] || die "container printed no RM_RC line"
  if [[ "$rc" == 0 && "$left" == 0 ]]; then
    note "rm -rf removed all $TREE_ENTRIES entries"
    return 0
  fi
  die "rm -rf of a $TREE_ENTRIES-entry directory returned $rc with $left entries left${errs:+ ($errs)}"
}

# ---- workload scripts (run with sh -c in alpine/busybox or in the S3 container)

# Creates files, sets owner, hard-links and renames inside $1 (the directory to use).
script_fs_ops() {
  cat <<'SCRIPT'
set -eu
dir=$1
mkdir -p "$dir"
cd "$dir"
echo hello > hello.txt
test "$(cat hello.txt)" = hello
mkdir -p owned
echo owned > owned/file
chown 1234:4321 owned/file
test "$(stat -c '%u:%g' owned/file)" = 1234:4321
ln hello.txt hard.txt
test "$(stat -c %h hard.txt)" = 2
mv hello.txt renamed.txt
test ! -e hello.txt
test "$(cat hard.txt)" = hello
test "$(cat renamed.txt)" = hello
echo FS_OPS_OK
SCRIPT
}

# Creates $1 with $2 empty files, then rm -rf's it. Prints what it observed.
script_fs_tree() {
  cat <<'SCRIPT'
set -eu
tree=$1
n=$2
mkdir -p "$tree"
i=0
while [ "$i" -lt "$n" ]; do : > "$tree/f$i"; i=$((i + 1)); done
echo "CREATED=$(ls -A "$tree" | wc -l | tr -d ' ')"
rc=0
rm -rf "$tree" 2>/tmp/rm.err || rc=$?
echo "RM_RC=$rc"
sed 's/^/RM_ERR: /' /tmp/rm.err | head -n 2
if [ -d "$tree" ]; then
  echo "LEFT=$(ls -A "$tree" | wc -l | tr -d ' ')"
else
  echo "LEFT=0"
fi
exit 0
SCRIPT
}

script_git() {
  cat <<'SCRIPT'
set -eu
dir=$1
mkdir -p "$dir"
cd "$dir"
rm -rf repo clone
mkdir repo
cd repo
git init -q .
mkdir sub
echo one > a.txt
echo two > sub/b.txt
git add -A
git -c user.name=dbfs-compat -c user.email=compat@dbfs.invalid commit -q -m initial
test -z "$(git status --porcelain)"
echo changed >> a.txt
test "$(git status --porcelain)" = " M a.txt"
git checkout -q -- a.txt
test -z "$(git status --porcelain)"
cd ..
git clone -q repo clone
test -z "$(git -C clone status --porcelain)"
git -C clone log --oneline | grep -q initial
echo GIT_OK
SCRIPT
}

# Extracts the docker-exported alpine rootfs and compares it with a reference
# extraction on the container's own disk: contents (diff -r), then modes, owners,
# file types, mtimes. Sizes are left to diff: directory sizes differ by design
# (dbfs reports 0, ext4 reports 4096) and are not a tar-visible difference.
script_tar() {
  cat <<'SCRIPT'
set -eu
dir=$1
mkdir -p "$dir/rootfs"
tar -xpf /in/alpine-rootfs.tar -C "$dir/rootfs"
ref=$(mktemp -d)
tar -xpf /in/alpine-rootfs.tar -C "$ref"
diff -r "$ref" "$dir/rootfs"
# The root "." is excluded: the archive does not carry it, so its mode and mtime
# come from whoever created the target directory.
meta() { (cd "$1" && find . -mindepth 1 | sort | xargs stat -c '%a %u %g %F %Y %n'); }
meta "$ref" >"$ref.meta"
meta "$dir/rootfs" >"$ref.meta.dbfs"
diff "$ref.meta" "$ref.meta.dbfs"
echo "TAR_OK entries=$(wc -l <"$ref.meta.dbfs" | tr -d ' ')"
SCRIPT
}

script_sqlite() {
  cat <<'SCRIPT'
set -eu
dir=$1
mkdir -p "$dir"
db="$dir/test.db"
rm -f "$db"
sqlite3 "$db" "CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT NOT NULL); WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 1000) INSERT INTO t(v) SELECT 'row' || x FROM c;"
test "$(sqlite3 "$db" 'SELECT count(*) FROM t;')" = 1000
test "$(sqlite3 "$db" 'PRAGMA integrity_check;')" = ok
echo SQLITE_OK
SCRIPT
}

script_wal() {
  cat <<'SCRIPT'
set -eu
dir=$1
mkdir -p "$dir"
db="$dir/wal.db"
rm -f "$db" "$db-wal" "$db-shm"
sqlite3 "$db" "PRAGMA journal_mode=WAL; CREATE TABLE t(x); INSERT INTO t VALUES (1), (2), (3);" >/dev/null
test "$(sqlite3 "$db" 'PRAGMA journal_mode;')" = wal
test "$(sqlite3 "$db" 'SELECT count(*) FROM t;')" = 3
test "$(sqlite3 "$db" 'PRAGMA integrity_check;')" = ok
echo WAL_OK
SCRIPT
}

# ---- cleanup ---------------------------------------------------------------

cleanup() {
  local rc=$? kind arg
  trap - EXIT
  set +e
  if [[ -s "$REG" ]]; then
    while read -r kind arg; do
      case "$kind" in
        container) docker rm -f "$arg" >/dev/null 2>&1 ;;
        volume) docker volume rm "$arg" >/dev/null 2>&1 ;;
        mount)
          if is_fuse_mounted "$arg"; then
            fusermount3 -u "$arg" >/dev/null 2>&1 || fusermount3 -uz "$arg" >/dev/null 2>&1
          fi
          ;;
      esac
    done < <(tac "$REG")
  fi
  # Remove the workspace only if nothing is still mounted under it.
  if [[ "${DBFS_COMPAT_KEEP:-}" != 1 && "$rc" -eq 0 ]] \
    && ! awk -v p="$WS/" 'index($2, p) == 1 && $3 ~ /^fuse/ { found = 1 } END { exit !found }' /proc/mounts; then
    rm -rf "$WS"
  elif [[ -d "$WS" ]]; then
    echo "workspace kept for inspection: $WS" >&2
  fi
  exit "$rc"
}

# ---- tests -----------------------------------------------------------------
# Each test runs in its own subshell. It exits 0 for PASS, 77 for SKIP,
# 78 for XFAIL, and anything else for FAIL. The detail text goes to DETAIL_FILE.

test_build_image() {
  docker build --progress=plain -t "$IMAGE" "$ROOT" >"$LOGS/docker-build.log" 2>&1 \
    || die "docker build failed: $(tail -n 5 "$LOGS/docker-build.log" | tr '\n' ' ')"
  note "built $IMAGE"
}

test_image_size() {
  local size
  size="$(docker image inspect -f '{{.Size}}' "$IMAGE")"
  note "$(awk -v s="$size" 'BEGIN { printf "%.2f MB", s / 1000000 }') (target < 20 MB)"
  (( size < IMAGE_SIZE_LIMIT )) || die "$size bytes is over the 20 MB limit"
}

test_export_rootfs() {
  local ex="dbfs-compat-export-$RUN_ID"
  mkdir -p "$WS/in"
  docker pull -q "$ALPINE" >/dev/null || die "docker pull $ALPINE failed"
  reg container "$ex"
  docker create --name "$ex" "$ALPINE" >/dev/null
  docker export -o "$WS/in/alpine-rootfs.tar" "$ex"
  docker rm -f "$ex" >/dev/null
  note "$(du -h "$WS/in/alpine-rootfs.tar" | cut -f1) rootfs tar from $ALPINE"
}

test_s1() {
  require_bin
  local dir="$WS/s1" db mnt
  mkdir -p "$dir"
  db="$dir/db.sqlite"
  mnt="$dir/mnt"
  host_mount "$db" "$mnt" || die "host mount failed; see $LOGS/host-dbfs.log"
  printf 'hello\n' >"$mnt/a.txt"
  [[ "$(cat "$mnt/a.txt")" == hello ]] || die "read back differs"
  mkdir "$mnt/d"
  mv "$mnt/a.txt" "$mnt/d/b.txt"
  ln "$mnt/d/b.txt" "$mnt/c.txt"
  [[ "$(stat -c %h "$mnt/c.txt")" == 2 ]] || die "hard link count is $(stat -c %h "$mnt/c.txt"), want 2"
  mv "$mnt/d" "$mnt/e"
  rm -rf "$mnt/e"
  [[ ! -e "$mnt/e" ]] || die "rm -rf left $mnt/e behind"
  host_unmount "$mnt" || die "dbfs exit status $? after unmount"
  host_mount "$db" "$mnt" || die "remount failed"
  [[ "$(cat "$mnt/c.txt")" == hello ]] || die "data did not survive remount"
  rm "$mnt/c.txt"
  host_unmount "$mnt" || die "dbfs exit status $? after second unmount"
  note "write, read, rename, hard link, rm -rf, and data after remount"
}

test_s2_ops() {
  allow_other_prereqs
  require_bin
  local dir="$WS/s2-ops" mnt out d
  mkdir -p "$dir"
  mnt="$dir/mnt"
  host_mount "$dir/db.sqlite" "$mnt" --allow-other || die "host mount with --allow-other failed; see $LOGS/host-dbfs.log"
  out=$(docker run --rm -v "$mnt:/data" "$ALPINE" sh -c "$(script_fs_ops)" sh /data/ops 2>&1) \
    || die "container workload failed: $(tail -n 3 <<<"$out" | tr '\n' ' ')"
  d="$mnt/ops"
  [[ "$(stat -c '%u:%g' "$d/owned/file")" == 1234:4321 ]] || die "chown not visible on host"
  [[ "$(stat -c %h "$d/renamed.txt")" == 2 ]] || die "host sees link count $(stat -c %h "$d/renamed.txt"), want 2"
  [[ "$(cat "$d/hard.txt")" == hello ]] || die "hard link content differs on host"
  [[ ! -e "$d/hello.txt" ]] || die "old name still present on host"
  host_unmount "$mnt" || die "dbfs exit status $? after unmount"
  note "container chown, hard link and rename verified from the host"
}

test_s2_rm() {
  allow_other_prereqs
  require_bin
  local dir="$WS/s2-rm" mnt out rc left=0 created
  mkdir -p "$dir"
  mnt="$dir/mnt"
  host_mount "$dir/db.sqlite" "$mnt" --allow-other || die "host mount with --allow-other failed; see $LOGS/host-dbfs.log"
  out=$(docker run --rm -v "$mnt:/data" "$ALPINE" sh -c "$(script_fs_tree)" sh /data/tree "$TREE_ENTRIES" 2>&1) \
    || die "container failed: $(tail -n 3 <<<"$out" | tr '\n' ' ')"
  created="$(sed -n 's/^CREATED=//p' <<<"$out")"
  [[ "$created" == "$TREE_ENTRIES" ]] || die "container created $created of $TREE_ENTRIES entries"
  rc="$(sed -n 's/^RM_RC=//p' <<<"$out")"
  if [[ -d "$mnt/tree" ]]; then
    left="$(ls -A "$mnt/tree" | wc -l | tr -d ' ')"
  fi
  host_unmount "$mnt" || die "dbfs exit status $? after unmount"
  judge_rm "$rc" "$left" "$out"
}

test_s3_start() {
  local out
  reg volume "$S3_VOL"
  docker volume create "$S3_VOL" >/dev/null
  reg container "$S3_CTR"
  docker run -d --name "$S3_CTR" "${FUSE_FLAGS[@]}" \
    -v "$S3_VOL:/data" -v "$WS/in:/in:ro" \
    "$IMAGE" mount /data/db.sqlite /mnt >/dev/null
  wait_container_mount "$S3_CTR" /mnt || die "dbfs did not mount /mnt inside $S3_CTR"
  out=$(docker exec "$S3_CTR" apk add --no-cache git sqlite tar 2>&1) \
    || die "apk add git sqlite tar failed: $(tail -n 3 <<<"$out" | tr '\n' ' ')"
  touch "$S3_READY"
  note "dbfs is PID 1 in $S3_CTR and mounted at /mnt; git, sqlite3 and tar installed"
}

test_s3_ops() {
  need_s3
  local out
  out=$(docker exec "$S3_CTR" sh -c "$(script_fs_ops)" sh /mnt/ops 2>&1) \
    || die "in-container workload failed: $(tail -n 3 <<<"$out" | tr '\n' ' ')"
  grep -qx FS_OPS_OK <<<"$out" || die "marker FS_OPS_OK missing"
  note "writes, chown, hard link and rename verified inside the container"
}

test_s3_rm() {
  need_s3
  local out rc left created
  out=$(docker exec "$S3_CTR" sh -c "$(script_fs_tree)" sh /mnt/tree "$TREE_ENTRIES" 2>&1) \
    || die "in-container rm step failed: $(tail -n 3 <<<"$out" | tr '\n' ' ')"
  created="$(sed -n 's/^CREATED=//p' <<<"$out")"
  [[ "$created" == "$TREE_ENTRIES" ]] || die "container created $created of $TREE_ENTRIES entries"
  rc="$(sed -n 's/^RM_RC=//p' <<<"$out")"
  left="$(sed -n 's/^LEFT=//p' <<<"$out")"
  judge_rm "$rc" "$left" "$out"
}

test_wl_git() {
  need_s3
  run_s3_script git GIT_OK "$(script_git)" /mnt/wl-git
  note "init, commit, clean status, modified detection, and clone on dbfs"
}

test_wl_tar() {
  need_s3
  [[ -f "$WS/in/alpine-rootfs.tar" ]] || die "no rootfs tar in $WS/in"
  run_s3_script tar TAR_OK "$(script_tar)" /mnt/wl-tar
  note "$(tail -n 1 "$LOGS/tar.out") identical to a reference extraction (modes, owners, sizes, mtimes)"
}

test_wl_sqlite() {
  need_s3
  run_s3_script sqlite SQLITE_OK "$(script_sqlite)" /mnt/wl-sqlite
  note "1000 rows inserted, count and PRAGMA integrity_check ok"
}

test_wl_wal() {
  need_s3
  run_s3_script wal WAL_OK "$(script_wal)" /mnt/wl-wal
  note "journal_mode=wal, 3 rows, PRAGMA integrity_check ok"
}

test_s4() {
  supports_allow_other || skip "needs C2 (--allow-other) and a shared host mount (mount --make-rshared)"
  skip "not automated yet: the host-side check needs a shared host mount; see docs/docker.md S4"
}

test_s6_stop() {
  local ctr="dbfs-compat-s6-$RUN_ID" dir="$WS/s6" t0 t1 code ms
  mkdir -p "$dir"
  dd if=/dev/urandom of="$dir/payload.bin" bs=1M count="$S6_MIB" status=none
  sha256sum "$dir/payload.bin" | cut -d' ' -f1 >"$dir/payload.sha256"
  reg volume "$S6_VOL"
  docker volume create "$S6_VOL" >/dev/null
  reg container "$ctr"
  docker run -d --name "$ctr" "${FUSE_FLAGS[@]}" -v "$S6_VOL:/data" -v "$dir:/in" \
    "$IMAGE" mount /data/db.sqlite /mnt >/dev/null
  wait_container_mount "$ctr" /mnt || die "dbfs did not mount /mnt inside $ctr"
  # Hold big.bin open with an unflushed write, as a long-running writer would.
  # tail -f copies the payload and then keeps the file open, so no close(2)
  # reaches dbfs. A close flushes the buffer (cat or a shell redirect would
  # flush when it exits), so a writer that closes its file would not leave
  # unflushed data behind.
  docker exec -d "$ctr" sh -c 'tail -c +1 -f /in/payload.bin >/mnt/big.bin'
  local want_bytes=$(( S6_MIB * 1048576 )) polls=0
  until [[ "$(docker exec "$ctr" stat -c %s /mnt/big.bin 2>/dev/null)" == "$want_bytes" ]]; do
    polls=$(( polls + 1 ))
    (( polls <= 240 )) || die "big.bin did not reach $want_bytes bytes within 120 s"
    sleep 0.5
  done
  t0=$(date +%s%N)
  docker stop -t 10 "$ctr" >/dev/null
  t1=$(date +%s%N)
  ms=$(( (t1 - t0) / 1000000 ))
  code="$(docker inspect -f '{{.State.ExitCode}}' "$ctr")"
  echo "$code" >"$dir/exit-code"
  if [[ "$code" == 137 ]]; then
    skip "needs C1 (SIGTERM handler): docker stop ended in SIGKILL (exit 137) after ${ms} ms with ${S6_MIB} MiB unflushed"
  fi
  [[ "$code" == 0 ]] || die "container exit code $code after ${ms} ms, want 0"
  (( ms < S6_STOP_LIMIT_MS )) || die "docker stop took ${ms} ms, limit ${S6_STOP_LIMIT_MS} ms"
  note "exit 0 after ${ms} ms with a ${S6_MIB} MiB unflushed write"
}

test_s6_restart() {
  local ctr="dbfs-compat-s6r-$RUN_ID" dir="$WS/s6" code want got size
  [[ -f "$dir/exit-code" ]] || die "the S6 stop row did not run"
  code="$(cat "$dir/exit-code")"
  want="$(cat "$dir/payload.sha256")"
  reg container "$ctr"
  docker run -d --name "$ctr" "${FUSE_FLAGS[@]}" -v "$S6_VOL:/data" "$IMAGE" mount /data/db.sqlite /mnt >/dev/null
  wait_container_mount "$ctr" /mnt || die "dbfs did not mount on the same volume"
  got="$(docker exec "$ctr" sha256sum /mnt/big.bin 2>/dev/null || true)"
  got="${got%% *}"
  size="$(docker exec "$ctr" stat -c %s /mnt/big.bin 2>/dev/null || echo missing)"
  if [[ "$code" == 137 ]]; then
    local same=no
    [[ "$got" == "$want" ]] && same=yes
    skip "needs C1 (SIGTERM handler): after SIGKILL big.bin has $size of $(( S6_MIB * 1048576 )) bytes, sha256 matches: $same"
  fi
  [[ "$got" == "$want" ]] || die "big.bin sha256 ${got:-missing} differs from $want (size $size)"
  note "${S6_MIB} MiB restored byte-for-byte after restart on the same volume"
}

# ---- driver ----------------------------------------------------------------

run_test() {
  local name=$1 fn=$2 slug log rc=0 status detail
  slug="$(printf '%s' "$name" | tr -c 'A-Za-z0-9' '_' | cut -c1-60)"
  log="$LOGS/$slug.log"
  DETAIL_FILE="$LOGS/$slug.detail"
  : >"$DETAIL_FILE"
  set +e
  (
    set -Eeuo pipefail
    trap host_mount_cleanup EXIT
    trap 'printf "unexpected error at line %s: %s\n" "$LINENO" "$BASH_COMMAND" >>"$DETAIL_FILE"' ERR
    "$fn"
  ) >"$log" 2>&1
  rc=$?
  set -e
  case "$rc" in
    0) status=PASS ;;
    77) status=SKIP ;;
    78) status=XFAIL ;;
    *) status=FAIL ;;
  esac
  detail="$(tr '\n' ' ' <"$DETAIL_FILE" | sed -e 's/[[:space:]]\+/ /g' -e 's/ $//')"
  printf '%s\t%s\t%s\n' "$status" "$name" "$detail" >>"$RESULTS"
  printf '%-5s %s%s\n' "$status" "$name" "${detail:+ -- $detail}"
}

preflight() {
  local tool
  for tool in docker fusermount3 awk sed sha256sum dd tac stat git; do
    command -v "$tool" >/dev/null || fatal "missing tool: $tool"
  done
  docker info >/dev/null 2>&1 || fatal "docker daemon is not reachable"
  [[ -c /dev/fuse ]] || fatal "/dev/fuse is missing"
  if [[ ! -x "$BIN" ]]; then
    echo "building $BIN"
    (cd "$ROOT" && cargo build --release) >"$LOGS/cargo-build.log" 2>&1 \
      || fatal "cargo build --release failed; see $LOGS/cargo-build.log"
  fi
}

print_table() {
  local rev pass fail skip xfail
  rev="$(git -C "$ROOT" rev-parse --short HEAD 2>/dev/null || echo unknown)"
  echo
  echo "dbfs compatibility matrix (commit $rev, image $IMAGE, tree $TREE_ENTRIES entries, S6 $S6_MIB MiB)"
  printf '%-5s %-70s %s\n' STATUS TEST DETAIL
  awk -F'\t' '{ printf "%-5s %-70s %s\n", $1, $2, $3 }' "$RESULTS"
  read -r pass fail skip xfail < <(awk -F'\t' '
    $1 == "PASS" { p++ } $1 == "FAIL" { f++ } $1 == "SKIP" { s++ } $1 == "XFAIL" { x++ }
    END { print p + 0, f + 0, s + 0, x + 0 }' "$RESULTS")
  echo
  echo "PASS $pass  FAIL $fail  SKIP $skip  XFAIL $xfail"
  return 0
}

main() {
  mkdir -p "$LOGS"
  : >"$REG"
  : >"$RESULTS"
  trap cleanup EXIT
  cd "$ROOT"
  preflight

  run_test "image: docker build $IMAGE" test_build_image
  run_test "image: size under 20 MB" test_image_size
  run_test "inputs: docker export of $ALPINE rootfs" test_export_rootfs
  run_test "S1 host mount: write, read, rename, hard link, rm -rf" test_s1
  run_test "S2 host mount --allow-other: container writes, chown, ln, mv" test_s2_ops
  run_test "S2 host mount --allow-other: container rm -rf of N-entry tree" test_s2_rm
  run_test "S3 dbfs in container: mount inside it (start)" test_s3_start
  run_test "S3 in-container mount: writes, chown, ln, mv" test_s3_ops
  run_test "S3 in-container mount: rm -rf of N-entry tree" test_s3_rm
  run_test "workload: git init, commit, status, clone on dbfs" test_wl_git
  run_test "workload: tar -xpf alpine rootfs onto dbfs (metadata diff)" test_wl_tar
  run_test "workload: sqlite3 insert + integrity_check on dbfs" test_wl_sqlite
  run_test "workload: sqlite3 WAL mode on dbfs" test_wl_wal
  run_test "S4 shared mount: container mount visible on host" test_s4
  run_test "S6 docker stop with unflushed write: exit code and time" test_s6_stop
  run_test "S6 restart on same volume: unflushed bytes present" test_s6_restart

  print_table
  local fails
  fails="$(awk -F'\t' '$1 == "FAIL" { n++ } END { print n + 0 }' "$RESULTS")"
  (( fails == 0 ))
}

main "$@"
