# Benchmark image: kernel filesystems, libfuse passthrough_hp and dbfs, all run
# from one privileged container against the same disk. Ubuntu matches the host
# so host-built dbfs and bench_io binaries run unchanged (bind-mounted repo).
FROM ubuntu:26.04
ARG LIBFUSE_TAG=fuse-3.18.2
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
      build-essential g++ meson ninja-build pkg-config git ca-certificates \
      libudev-dev liburing-dev libnuma-dev fio btrfs-progs e2fsprogs fuse3 util-linux python3 sqlite3 \
    && rm -rf /var/lib/apt/lists/*
RUN git clone --depth 1 --branch "$LIBFUSE_TAG" https://github.com/libfuse/libfuse /src/libfuse \
    && cd /src/libfuse && meson setup build -Dexamples=true -Dtests=false -Dutils=false \
    && ninja -C build example/passthrough_hp \
    && install -m755 build/example/passthrough_hp /usr/local/bin/passthrough_hp \
    && ln -s /src/libfuse/build/lib/libfuse3.so* /usr/local/lib/ \
    && ldconfig
