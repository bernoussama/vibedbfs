#!/usr/bin/env bash
# Run cargo inside a Rust container, for hosts without a C toolchain.
#
#   scripts/cargo-docker.sh build --release
#   scripts/cargo-docker.sh test
#   DBFS_RUN_FUSE_TESTS=1 scripts/cargo-docker.sh test --test mount_integration -- --ignored
#
# The worktree is bind-mounted, so build output lands in ./target as on the host.
# The cargo registry is cached in a named volume between runs. The resulting
# binaries link glibc and libsqlite3.so.0 dynamically and run on any host with a
# glibc at least as new as Debian trixie's.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
image="dbfs-build:trixie-2"

if ! docker image inspect "$image" >/dev/null 2>&1; then
  docker build -q -t "$image" - >/dev/null <<'EOF'
FROM rust:1-trixie
RUN apt-get update \
 && apt-get install -y --no-install-recommends libsqlite3-dev pkg-config fuse3 strace python3-pytest \
 && rm -rf /var/lib/apt/lists/* \
 && rustup component add rustfmt clippy \
 && mkdir -p /cargo/registry && chmod -R 0777 /cargo
EOF
fi

docker volume create dbfs-cargo-registry >/dev/null
# /dev/fuse lets the FUSE mount tests run inside the container too.
exec docker run --rm \
  --device /dev/fuse --cap-add SYS_ADMIN --security-opt apparmor=unconfined \
  -e DBFS_RUN_FUSE_TESTS \
  --user "$(id -u):$(id -g)" \
  -e CARGO_HOME=/cargo \
  -v dbfs-cargo-registry:/cargo/registry \
  -v "$repo_root":"$repo_root" \
  -w "$repo_root" \
  "$image" cargo "$@"
