# syntax=docker/dockerfile:1
#
# dbfs image: one static musl binary plus fusermount3, on Alpine.
#
#   docker build -t dbfs:dev .
#   docker run --rm dbfs:dev                 # prints usage (the default CMD)
#
# Build stage: Rust on Alpine (musl). SQLite is compiled into the binary
# (feature `bundled-sqlite`), so the runtime image needs no libsqlite3.
# Runtime stage: Alpine, fuse3 (fusermount3), and the binary. Nothing else.
# See docs/docker.md for run flags, Compose, and the Linux/Docker limitations.

ARG RUST_IMAGE=rust:1-alpine
ARG ALPINE_IMAGE=alpine:3.24
ARG DBFS_VERSION=0.1.0

# ---- build -----------------------------------------------------------------
FROM ${RUST_IMAGE} AS build

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src

# The registry and target directory are BuildKit cache mounts, so rebuilds
# reuse them. They are not part of any layer, so the binary is copied out of
# the target cache within the same step.
#
# CARGO_PROFILE_RELEASE_STRIP keeps the image small; it is set here, not in
# Cargo.toml, so local release builds keep their symbols.
#
# The `readelf` check fails the build if the binary has any NEEDED shared
# library. The image is only correct if dbfs is fully static.
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/src/target,sharing=locked \
    CARGO_PROFILE_RELEASE_STRIP=symbols \
    cargo build --release --locked --features bundled-sqlite \
 && install -D -m 0755 target/release/dbfs /out/usr/local/bin/dbfs \
 && if readelf -d /out/usr/local/bin/dbfs | grep -q '(NEEDED)'; then \
      echo "dbfs links shared libraries; expected a static binary:" >&2; \
      readelf -d /out/usr/local/bin/dbfs | grep NEEDED >&2; \
      exit 1; \
    fi

# ---- runtime ---------------------------------------------------------------
FROM ${ALPINE_IMAGE}

ARG DBFS_VERSION

LABEL org.opencontainers.image.title="dbfs" \
      org.opencontainers.image.description="FUSE filesystem backed by SQLite" \
      org.opencontainers.image.version="${DBFS_VERSION}" \
      org.opencontainers.image.source="https://github.com/bernoussama/vibedbfs"

# fuse3 provides fusermount3, which fuser falls back to when mount(2) is not
# allowed (the fallback is used for non-root users).
RUN apk add --no-cache fuse3

COPY --from=build /out/usr/local/bin/dbfs /usr/local/bin/dbfs

# SIGTERM from `docker stop` is delivered to dbfs, which is PID 1 here, so
# dbfs must handle it (see the shutdown work in docs/docker.md).
STOPSIGNAL SIGTERM

# Exec form: dbfs is PID 1 and receives signals directly, with no shell in between.
ENTRYPOINT ["dbfs"]
CMD ["--help"]
