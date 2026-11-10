# Running dbfs in Docker

This guide covers the `dbfs:dev` image and these ways to run dbfs with containers:

- S2: a host mount shared into a container.
- S3: dbfs running inside a container, mounting inside it.
- S4: the in-container mount shared back to the host.
- S5: rootless Podman.
- S6: stopping and shutdown.

The scenario IDs come from the [compatibility requirements](plans/2026-11-03-data-oriented-redesign.md#linux-and-docker-compatibility-requirements) in the redesign plan.

The changes these steps depend on (C1, C2, C4 and C5 in the redesign plan) are all merged. The [status table](#changes-these-steps-depend-on) lists them, together with the work that is still pending.

## Overview

The image has two stages:

- **Build:** `rust:1-alpine` (musl). It runs `cargo build --release --locked --features bundled-sqlite`, which compiles SQLite into the binary. The build fails if the binary links any shared library.
- **Runtime:** `alpine:3.24` with `fuse3` (which provides `fusermount3`) and `/usr/local/bin/dbfs`. Nothing else.

`dbfs` is the entrypoint, in exec form, so it runs as PID 1 and receives signals directly. The default command is `--help`, so running the image with no arguments prints usage:

```bash
docker run --rm dbfs:dev
```

The image runs as root. fuser, the FUSE library dbfs uses, first calls `mount(2)` directly and falls back to `fusermount3` only when that call is denied. A root container with `--cap-add SYS_ADMIN` therefore mounts directly and does not need the setuid helper. Non-root users use the setuid `fusermount3` fallback.

## Build

```bash
docker build -t dbfs:dev .
```

The Dockerfile uses BuildKit cache mounts, so build with BuildKit (the default in current Docker releases).

- **Image size:** 18.5 MB (18,497,396 bytes) for `dbfs:dev`, measured with `docker image inspect -f '{{.Size}}' dbfs:dev`. The target is under 20 MB. Most of it is Alpine itself (about 12 MB with fuse3); the binary is 3.2 MB, stripped and static.
- **Platforms:** the image has been built and checked for linux/amd64. linux/arm64 is not verified yet.

Check that the binary is in the image:

```bash
docker run --rm --entrypoint sh dbfs:dev -c 'ls -l /usr/local/bin/dbfs'
```

This shows that the binary exists and how large it is. It does not prove static linking. The build does that: the `readelf` check in the Dockerfile fails the build if the binary has any `NEEDED` entry. To list the shared-library entries yourself, copy the binary out and run `readelf` on the host:

```bash
docker create --name dbfs-extract dbfs:dev
docker cp dbfs-extract:/usr/local/bin/dbfs ./dbfs-static
docker rm dbfs-extract
readelf -d ./dbfs-static | grep NEEDED || echo "no shared libraries"
rm ./dbfs-static
```

To build without a C toolchain on the host, use `scripts/cargo-docker.sh`. It runs cargo in a Rust container, so only Docker is needed on the host:

```bash
scripts/cargo-docker.sh build --release --features bundled-sqlite
scripts/cargo-docker.sh test
```

The first run builds a helper image, `dbfs-build:trixie-2`, and creates the `dbfs-cargo-registry` volume. Output goes to `./target`, as with a host build. These binaries link glibc dynamically, so they are not the static musl binary that the image ships.

## Changes these steps depend on

| Change | Status | What it provides | Needed for |
|---|---|---|---|
| C1 | Merged | Graceful shutdown: SIGTERM, SIGINT and SIGHUP flush, unmount and exit 0 (exit 1 if the flush fails); `Filesystem::destroy` flushes too | Clean `docker stop` (S3, S4, S6) |
| C2 | Merged | `--allow-other`, `--allow-root`, `--auto-unmount`, `-o OPT` | Containers and other users on a host mount (S2, S4) |
| C4 | Merged | Explicit xattr behaviour | `cp -a`, `tar --xattrs`, `rsync -X` |
| C5 | Merged | `fallocate`, mode 0 and `FALLOC_FL_KEEP_SIZE` | Package managers, databases and `fio` in containers (S2, S3) |

Still pending:

- **S4 automation.** The host-side check in `tests/compat/run.sh` is not written yet, so the S4 row is always SKIP.
- **S5 (rootless Podman).** Best effort. It is not in CI yet.
- **aarch64.** linux/arm64 has not been built or tested.

The `mount` syntax:

```text
dbfs mount [--allow-other | --allow-root] [--auto-unmount] [--write-buffer SIZE] [--kernel-cache] [-o OPT[,OPT...]]... <DATABASE> <MOUNTPOINT>
```

## S2: host mount shared into a container

The host mounts dbfs, and containers reach the same files through a bind mount. The mount must exist before the container starts.

This needs `--allow-other`. Without it, only the user who mounted dbfs can access the mount, so root inside a container gets `EACCES` and the bind mount does not work.

Run this once on the host:

```bash
sudo sed -i 's/^#\?user_allow_other/user_allow_other/' /etc/fuse.conf
sudo mkdir -p /srv/dbfs
```

- `user_allow_other` lets non-root users pass `--allow-other` through `fusermount3`. Root does not need it. If `/etc/fuse.conf` has no `user_allow_other` line at all, the `sed` changes nothing, so add the line by hand.
- If you are not root, you need write access to `/srv/dbfs` and to the directory that holds the database. Run `sudo chown "$USER" /srv/dbfs`, or keep the database in a directory you own.

Start the mount. It runs in the foreground until it is unmounted, so use a separate terminal or tmux pane (or add `&`):

```bash
dbfs mount --allow-other /srv/dbfs.sqlite /srv/dbfs
```

Add `--auto-unmount` so that a killed dbfs does not leave a stale mount behind. It needs `--allow-other` or `--allow-root`, and dbfs refuses it without one. On Ubuntu hosts, the AppArmor profile shipped for `fusermount3` denies the `rprivate` remount that the cleanup needs, so a killed dbfs can still leave a stale mount there. Unmount it with `fusermount3 -u <mnt>`.

Now use the mount from a container, then read the file on the host:

```bash
docker run --rm -v /srv/dbfs:/data alpine sh -c 'echo hi > /data/hello.txt'
cat /srv/dbfs/hello.txt
```

When you are finished, unmount:

```bash
fusermount3 -u /srv/dbfs
```

Ctrl-C, `kill` with SIGTERM, SIGINT or SIGHUP, and `docker stop` all flush buffered writes and unmount before dbfs exits. Only SIGKILL cannot be caught. The kernel sends a flush request on every `close(2)` of a file, and dbfs writes that file's buffer out when it gets it. So a program that closes its files before dbfs stops loses nothing. Writes that a program still holds open and has not closed are lost only if dbfs is killed with SIGKILL.

## S3: dbfs inside a container, mounting inside it

Start dbfs in a container. It mounts `/mnt` inside the container, and its database lives in a named volume:

```bash
docker run --rm --device /dev/fuse --cap-add SYS_ADMIN --security-opt apparmor=unconfined -v dbdata:/data dbfs:dev mount /data/db.sqlite /mnt
```

What each flag does:

- `--device /dev/fuse` puts the FUSE device node into the container.
- `--cap-add SYS_ADMIN` gives the container the capability that `mount(2)` needs.
- `--security-opt apparmor=unconfined`: on Ubuntu, the default AppArmor profile denies mounts inside containers. This turns that profile off for this container.
- `-v dbdata:/data` keeps the database in a named volume.
- The arguments after the image name replace the default command and are passed to the `dbfs` entrypoint.

The container stays in the foreground until it is stopped. Open a second terminal to use the mount from inside.

**(a) `docker exec` into the running container.** `docker exec` joins the container's mount namespace, so `/mnt` is the dbfs mount there:

```bash
docker ps
docker exec <container> sh -c 'echo hi > /mnt/hello.txt && cat /mnt/hello.txt'
```

Use the name or ID from `docker ps` for `<container>`.

**(b) A wrapper entrypoint.** The wrapper starts dbfs in the background, waits until `/mnt` is mounted, runs your command, and then stops dbfs. Save it as `wrap.sh`:

```sh
#!/bin/sh
# Mount dbfs on /mnt, run the command given as arguments, then stop dbfs.
set -eu

dbfs mount /data/db.sqlite /mnt &
dbfs_pid=$!

until grep -q " /mnt fuse" /proc/mounts; do
  kill -0 "$dbfs_pid" 2>/dev/null || { echo "dbfs exited before mounting" >&2; exit 1; }
  sleep 0.2
done

"$@" &
app_pid=$!

# docker stop sends SIGTERM to this shell (PID 1). Pass it on to the app.
# A trapped signal makes `wait` return early, so loop until the app has exited.
trap 'kill -TERM "$app_pid" 2>/dev/null || true' TERM INT
status=0
while :; do
  wait "$app_pid" && status=0 || status=$?
  kill -0 "$app_pid" 2>/dev/null || break
done

# Stop dbfs last, after the app has finished.
kill -TERM "$dbfs_pid" 2>/dev/null || true
wait "$dbfs_pid" || true
exit "$status"
```

Run your command through it. `--entrypoint /bin/sh` replaces the `dbfs` entrypoint for this run, so the script runs instead:

```bash
docker run --rm --init --device /dev/fuse --cap-add SYS_ADMIN --security-opt apparmor=unconfined \
  -v dbdata:/data -v "$PWD/wrap.sh:/wrap.sh:ro" \
  --entrypoint /bin/sh dbfs:dev /wrap.sh sh -c 'echo hi > /mnt/hello.txt && cat /mnt/hello.txt'
```

A plain `sh` as PID 1 ignores SIGTERM unless it installs a handler, which is why the script has a `trap`. `--init` still helps: it reaps children and forwards signals to the script. The wrapper exits when the app does, so `docker stop` finishes in time. The wrapper then sends SIGTERM to dbfs, which flushes and unmounts before it exits.

## S4: sharing the in-container mount with the host

A mount made inside a container reaches the host only when the host path is a shared mount and the container's bind mount propagates. Mount propagation is set at runtime, so it does not survive a reboot.

Check the host path first:

```bash
sudo mkdir -p /srv/dbfs
findmnt -no PROPAGATION -T /srv/dbfs
```

If it prints `shared`, nothing else is needed. On systemd hosts the root mount is shared, so paths under `/` already are. This was checked on the development machine: a tmpfs mount made inside a container with `-v /path:/x:rshared` appeared on the host, with no `--make-rshared` on the path. If the command prints `private`, which happens when `/srv` is on its own private mount, make the path shared:

```bash
sudo mount --bind /srv/dbfs /srv/dbfs && sudo mount --make-rshared /srv/dbfs
```

Start dbfs in a container, with the host path bound to `/mnt`:

```bash
docker run -d --name dbfs-share --device /dev/fuse --cap-add SYS_ADMIN --security-opt apparmor=unconfined -v dbdata:/data -v /srv/dbfs:/mnt:rshared dbfs:dev mount /data/db.sqlite /mnt
```

Check the mount from the host:

```bash
ls /srv/dbfs
docker exec dbfs-share sh -c 'echo hi > /mnt/hello.txt'
cat /srv/dbfs/hello.txt
```

Both sides need propagation:

- The host side must be shared. `findmnt` shows this, and `--make-rshared` (above) fixes it when it is not.
- `:rshared` on the bind mount lets mounts that the container creates under `/mnt` propagate back to the host. Without it (`-v /srv/dbfs:/mnt` alone), a mount made inside the container may stay there. This was not tested here.

Access: host root can read the mount, because the container's root is host root and made the mount. Other host users need `--allow-other` on the in-container mount, for example `dbfs:dev mount --allow-other /data/db.sqlite /mnt`. Without it, only root can access the mount. `user_allow_other` in `/etc/fuse.conf` affects only `fusermount3`. This container mounts as root and does not use `fusermount3`, so this scenario does not need that setting.

When you are finished, stop the container and check that the mount is gone:

```bash
docker stop dbfs-share
findmnt /srv/dbfs
```

If `findmnt` still lists a dbfs mount on `/srv/dbfs`, remove it with `sudo umount -l /srv/dbfs`.

## S5: rootless Podman (best effort)

Rootless Podman can run the same image, but this is best effort and is not checked on every change. Requirements:

- Linux kernel 4.18 or newer.
- `/dev/fuse` readable and writable by your user.
- On SELinux hosts, add `--security-opt label=disable` to the run command.

```bash
podman build -t dbfs:dev .
podman run --rm --device /dev/fuse --cap-add SYS_ADMIN --security-opt apparmor=unconfined -v dbdata:/data dbfs:dev mount /data/db.sqlite /mnt
```

`podman build` reads the same Dockerfile and tags the image the same way as `docker build`.

## S6: stopping and shutdown

`docker stop` sends SIGTERM to PID 1, waits for the stop timeout (10 s by default), and then sends SIGKILL. A flush can take longer than that. Raise the timeout with `--stop-timeout` when you start the container, or with `-t` when you stop it:

```bash
docker run --rm --stop-timeout 30 --device /dev/fuse --cap-add SYS_ADMIN --security-opt apparmor=unconfined -v dbdata:/data dbfs:dev mount /data/db.sqlite /mnt
docker stop -t 30 <container>
```

`--init` runs tini as PID 1, and tini reaps zombie processes and forwards signals. dbfs starts no long-lived child processes, so reaping rarely matters for dbfs itself. `--init` is still a good default for wrapper entrypoints such as the one in S3(b), because it forwards signals to them:

```bash
docker run --rm --init --device /dev/fuse --cap-add SYS_ADMIN --security-opt apparmor=unconfined -v dbdata:/data dbfs:dev mount /data/db.sqlite /mnt
```

dbfs handles SIGTERM even as PID 1, so `docker stop` ends with a clean shutdown: dbfs flushes buffered writes, detaches the mount and exits 0. If a flush fails, dbfs prints `failed to flush unsaved data on shutdown` to stderr and exits 1, so the failure is visible. If a process still has a file open, the kernel keeps the FUSE session alive after the detach. dbfs then exits when a one-second grace period runs out. The data is already flushed by then, and new writes fail, so nothing is lost.

The S6 row in `tests/compat/run.sh` checks this with a 16 MiB write that is held open and not flushed. The stop must exit 0 in under 5 s, and after a restart the file must match byte for byte. An exit code of 137 (SIGKILL) makes that row FAIL. Check the exit code after a stop:

```bash
docker inspect -f '{{.State.ExitCode}}' <container>
```

## Docker Compose

This Compose file runs dbfs as a service:

```yaml
services:
  dbfs:
    image: dbfs:dev
    command: ["mount", "/data/db.sqlite", "/mnt"]
    devices: ["/dev/fuse:/dev/fuse"]
    cap_add: [SYS_ADMIN]
    security_opt: ["apparmor=unconfined"]
    volumes: ["dbdata:/data"]
    stop_grace_period: 30s

volumes:
  dbdata:
```

Start it, use the mount, and stop it:

```bash
docker compose up -d
docker compose exec dbfs sh -c 'echo hi > /mnt/hello.txt && cat /mnt/hello.txt'
docker compose stop
```

`stop_grace_period` is the Compose equivalent of `--stop-timeout`. Raise it only if a stop needs more than the default 10 s.

## Limitations

- **(a) dbfs cannot be Docker's `data-root`.** Docker's overlay2 driver uses the storage as an overlayfs upperdir, and the kernel's overlayfs does not support a FUSE upperdir. Volumes and bind mounts are fine.
- **(b) No SELinux relabeling.** `:z` and `:Z` do not work on dbfs. On SELinux hosts, use `--security-opt label=disable`.
- **(c) xattrs.** dbfs stores no extended attributes. `getxattr` and `removexattr` return `ENODATA`, `listxattr` returns an empty list, and `setxattr` returns `EOPNOTSUPP`. Tools then treat the filesystem as having no xattrs. Values such as `security.capability` on binaries therefore cannot be kept.
- **(d) Access by other users.** Only the user who started dbfs can access the mount, unless it was started with `--allow-other` (every user) or `--allow-root` (root, as well as the mounting user). The two flags cannot be combined. For a non-root user, `--allow-other` also needs `user_allow_other` in `/etc/fuse.conf`.
- **(e) Durability.** The database lives in a volume and is only as durable as its last flush. File data is held in memory and written to the database on `flush` (the kernel sends one on every `close(2)`), on `fsync`, on `release`, and before an attribute change such as `chmod` or `truncate`. It is also written when the shared write buffer is full. `--write-buffer` sets its size (64 MiB by default), and the file with the most buffered data is written first. Data for a file that is unlinked, or replaced by a rename, is dropped, not written. A SIGTERM, SIGINT or SIGHUP flushes everything before dbfs exits. A SIGKILL cannot be caught, so writes that a program still holds open and has not closed are lost then. See [Storage in the README](../README.md#storage).
- **(f) fallocate.** Mode 0 and `FALLOC_FL_KEEP_SIZE` work. Mode 0 extends the file with zeros, which read back as zeros. Neither reserves space, because SQLite allocates pages on write. Punch hole and zero range fail with `EOPNOTSUPP`. `posix_fallocate` uses mode 0, so it works.
