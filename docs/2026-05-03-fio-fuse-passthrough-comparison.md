# fio FUSE Passthrough Comparison

Date: 2026-05-03

## Goal

Compare dbfs against native filesystems and libfuse passthrough baselines.

The passthrough runs are the closest apples-to-apples comparison for dbfs because both paths pay FUSE overhead:

```text
fio -> FUSE -> passthrough -> filesystem
fio -> FUSE -> dbfs        -> SQLite -> filesystem
```

## Workload

All runs used:

```text
size=512M
bs=4k
rw=randwrite
fsync=0
ioengine=sync
```

Equivalent fio shape:

```bash
fio --name=<name> \
  --directory=<directory> \
  --size=512M \
  --bs=4k \
  --rw=randwrite \
  --fsync=0 \
  --ioengine=sync
```

The FUSE passthrough baseline used libfuse `passthrough_ll` with:

```text
no_writeback
cache=never
```

## btrfs Results

btrfs was the project filesystem:

```text
/dev/nvme0n1p6 btrfs
```

| Target | Runtime | Bandwidth | IOPS | Avg write latency | p50 | p95 | p99 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| native btrfs | 0.770s | 665 MiB/s | 170k | 4.94 us | 5 us | 8 us | 18 us |
| FUSE passthrough btrfs | 2.322s | 220 MiB/s | 56.4k | 16.57 us | 15 us | 24 us | 45 us |
| dbfs on btrfs | 3.166s | 162 MiB/s | 41.4k | 23.09 us | 21 us | 36 us | 57 us |

In this no-fsync workload, dbfs reached about 74% of FUSE passthrough btrfs throughput:

```text
162 MiB/s / 220 MiB/s = 0.736
```

## ext4 Results

The ext4 test used a temporary ext4 loop image. dbfs was not run on ext4 for this comparison; these numbers are only fio against native ext4-loop and FUSE passthrough ext4-loop.

| Target | Runtime | Bandwidth | IOPS | Avg write latency | p50 | p95 | p99 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| native ext4 loop | 0.365s | 1403 MiB/s | 359k | 2.05 us | 1.67 us | 3.70 us | 5.47 us |
| FUSE passthrough ext4 loop | 1.787s | 287 MiB/s | 73.3k | 12.66 us | 12 us | 19 us | 36 us |

## Caveats

The ext4 filesystem was mounted from a loop image backed by the btrfs project disk. It is useful for comparing native ext4-loop to FUSE passthrough ext4-loop, but it is not as clean as benchmarking a real ext4 partition.

The dbfs result uses the buffered-write implementation, where `write` stores dirty data in memory and `flush`, `fsync`, or `release` writes it to SQLite.

The workload uses `fsync=0`, so these results measure buffered random-write throughput, not durable-write throughput.
