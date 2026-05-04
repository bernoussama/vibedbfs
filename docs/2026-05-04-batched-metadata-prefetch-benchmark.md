# Batched Metadata and Prefetch Benchmark

Date: 2026-05-04

## Summary

This run measured the `batch-metadata-prefetch` branch after adding batched
metadata transactions, multi-chunk range reads, read-ahead caching, and FUSE
writeback-cache capability negotiation.

The benchmark data was placed under the repository `.bench/` directory so the
workload used disk-backed storage instead of `/tmp`, which is `tmpfs` on this
machine.

```text
Project filesystem: /dev/nvme0n1p6 btrfs /home/oussama/projects/dbfs
/tmp filesystem: tmpfs
Kernel: Linux 6.19.13-arch1-1 x86_64
fio: fio-3.41
libfuse: 3.18.2
```

## Correctness Check

The Rust suite passed after the writeback-cache fix:

```bash
cargo test
```

The Python/libfuse harness was not run because `pytest` is not installed in
this environment:

```text
pytest: command not found
python3: No module named pytest
```

## Default dbfs fio Run

Command:

```bash
TMPDIR=/home/oussama/projects/dbfs/.bench scripts/bench-fio.sh
```

Workload:

```text
rw=randwrite
size=512M
bs=4k
ioengine=sync
iodepth=1
numjobs=1
fsync=1
```

Result:

| Workload | Bandwidth | IOPS | Runtime |
| --- | ---: | ---: | ---: |
| Random write, fsync=1 | 4133 KiB/s | 1033 | 126.856s |

## FUSE Passthrough Comparison

The comparison used the same short smoke-suite shape as the previous
disk-backed passthrough benchmark:

```text
size=64M
bs=4k
ioengine=sync
iodepth=1
numjobs=1
fsync=0
```

The dbfs side used:

```bash
TMPDIR=/home/oussama/projects/dbfs/.bench \
  scripts/bench-fio.sh --job suite --size 64M --bs 4k --fsync 0 \
  -- --group_reporting
```

The passthrough side used libfuse `passthrough_ll` compiled from the local
`/home/oussama/clones/libfuse` checkout:

```bash
gcc -O2 /home/oussama/clones/libfuse/example/passthrough_ll.c \
  -o /home/oussama/projects/dbfs/.bench/passthrough_ll \
  $(pkg-config --cflags --libs fuse3) -lpthread

/home/oussama/projects/dbfs/.bench/passthrough_ll \
  -o source=/home/oussama/projects/dbfs/.bench/passthrough-fio.wcrkNa/src \
  -o no_writeback \
  -o cache=never \
  /home/oussama/projects/dbfs/.bench/passthrough-fio.wcrkNa/mnt
```

Each passthrough fio job used:

```bash
fio \
  --name="passthrough-$rw" \
  --directory=/home/oussama/projects/dbfs/.bench/passthrough-fio.wcrkNa/mnt \
  --size=64M \
  --bs=4k \
  --rw="$rw" \
  --ioengine=sync \
  --iodepth=1 \
  --numjobs=1 \
  --fsync=0 \
  --group_reporting
```

## Results

| Workload | dbfs bandwidth | dbfs IOPS | Passthrough bandwidth | Passthrough IOPS | dbfs / passthrough |
| --- | ---: | ---: | ---: | ---: | ---: |
| Sequential write | 410 MiB/s | 105k | 44.1 MiB/s | 11.3k | 9.30x |
| Random write | 340 MiB/s | 87.1k | 37.6 MiB/s | 9.63k | 9.04x |
| Sequential read | 342 MiB/s | 87.6k | 84.7 MiB/s | 21.7k | 4.04x |
| Random read | 26.5 MiB/s | 6.80k | 116 MiB/s | 29.6k | 0.23x |

## Notes

- `dbfs` is substantially faster than passthrough for writes in this no-fsync
  smoke suite, helped by userspace buffering and SQLite batching.
- Sequential reads improved relative to passthrough in this run, but random
  reads remain the weak workload: passthrough was about 4.4x faster.
- The attempted `writeback_cache` mount option failed with `fusermount3:
  unknown option 'writeback_cache'`. The fix was to request
  `FUSE_WRITEBACK_CACHE` in `Filesystem::init` and enable `fuser`'s
  `abi-7-23` feature.
- These are short 64 MiB smoke-style results. Larger repeated runs are needed
  before treating the numbers as publication-grade.
