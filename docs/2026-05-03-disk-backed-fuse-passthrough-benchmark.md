# Disk-backed FUSE Passthrough Benchmark

Date: 2026-05-03

## Summary

This run compares `dbfs` against libfuse `passthrough_ll` on the same
disk-backed filesystem. Earlier ad hoc runs under `/tmp` are not disk
benchmarks on this machine because `/tmp` is mounted as `tmpfs`.

Both benchmark targets used temporary directories under the repository's
`.bench/` directory, which is backed by the project filesystem:

```text
/dev/nvme0n1p6 btrfs /home/oussama/projects/dbfs
```

## Machine

```text
Host: archbtw
Kernel: Linux 6.19.13-arch1-1 x86_64
CPU: Intel(R) Core(TM) i7-9750H CPU @ 2.60GHz
Cores/threads: 6 cores, 12 threads
Memory: 30 GiB RAM, 4 GiB swap
Disk: SSDPEMKF010T8 NVMe INTEL 1024GB
Project filesystem: btrfs on /dev/nvme0n1p6
/tmp filesystem: tmpfs
```

Tool versions:

```text
fio: fio-3.41
libfuse: 3.18.2
FUSE kernel interface: 7.45
```

## Workload

All runs used:

```text
size=64M
bs=4k
ioengine=sync
iodepth=1
numjobs=1
fsync=0
```

The workload suite ran:

```text
write
randwrite
read
randread
```

## Commands

`dbfs` was run through the repository benchmark helper with explicit
disk-backed paths:

```bash
mkdir -p .bench
root=$(mktemp -d /home/oussama/projects/dbfs/.bench/dbfs-fio.XXXXXX)
mkdir -p "$root/mnt"
scripts/bench-fio.sh \
  --job suite \
  --size 64M \
  --bs 4k \
  --fsync 0 \
  --database "$root/dbfs.sqlite" \
  --mountpoint "$root/mnt" \
  -- --group_reporting
```

The passthrough run used upstream libfuse `passthrough_ll` with writeback and
kernel caching disabled:

```bash
/tmp/passthrough_ll \
  -o source="$root/src" \
  -o no_writeback \
  -o cache=never \
  "$root/mnt"
```

Each passthrough fio job used this shape:

```bash
fio \
  --name="passthrough-$rw" \
  --directory="$mnt" \
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
| Sequential write | 358 MiB/s | 91.5k | 292 MiB/s | 74.8k | 1.23x |
| Random write | 372 MiB/s | 95.3k | 261 MiB/s | 66.9k | 1.43x |
| Sequential read | 1306 MiB/s | 334k | 393 MiB/s | 101k | 3.32x |
| Random read | 101 MiB/s | 26.0k | 352 MiB/s | 90.0k | 0.29x |

## Notes

- These are short 64 MiB smoke-style benchmarks, useful for checking whether a
  change causes a large regression. Larger runs are needed for publication-grade
  results.
- `dbfs` outperformed passthrough on writes in this workload shape. This is
  expected to depend heavily on buffering behavior and `fsync=0`.
- Random reads remain much slower in `dbfs` than in passthrough for this run.
- Do not compare these disk-backed numbers with `/tmp` runs. On this machine
  `/tmp` is RAM-backed `tmpfs`.
