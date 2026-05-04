# dbfs fio Benchmark Results

Date: 2026-05-03

## Workload

Both runs used the same disk-backed scratch area on the project filesystem:

```text
/home/oussama/projects/dbfs/.fio-disk.xiCw2w
```

Filesystem:

```text
/dev/nvme0n1p6 btrfs
```

fio profile:

```bash
fio --name=<name> \
  --directory=<test-directory> \
  --size=512M \
  --bs=4k \
  --rw=randwrite \
  --fsync=1 \
  --ioengine=sync
```

The dbfs run used:

```bash
scripts/bench-fio.sh \
  --database /home/oussama/projects/dbfs/.fio-disk.xiCw2w/dbfs.sqlite \
  --mountpoint /home/oussama/projects/dbfs/.fio-disk.xiCw2w/dbfs-mnt \
  --job randwrite \
  --size 512M \
  --bs 4k \
  --fsync 1
```

## Results

| Target | Runtime | Bandwidth | IOPS | Avg write latency | p50 | p95 | p99 |
| --- | --- | --- | --- | --- | --- | --- | --- |
| dbfs | 49.39s | 10.4 MiB/s | 2,653 | 373 us | 149 us | 457 us | 1,336 us |
| native btrfs | 167.14s | 3.06 MiB/s | 784 | 13.65 us | 10 us | 34 us | 57 us |

For this exact fio command, dbfs showed about 3.4x higher throughput and IOPS than native btrfs.

## Important Caveat

This is not a fair durability comparison yet.

The native btrfs run spent most of its time in real fsync work:

```text
native fsync avg: 1257.29 us
```

The dbfs run reported fsync latency in nanoseconds:

```text
dbfs fsync avg: 1240.37 ns
```

That strongly suggests fsync is effectively a no-op through dbfs right now. The dbfs result is useful as a current performance datapoint, but it should not be read as equivalent to native btrfs durability until dbfs implements and verifies durable fsync behavior.

## Previous tmpfs Comparison

An earlier native comparison used `/tmp`, which is mounted as `tmpfs` on this machine. That produced much faster native numbers, but it compared dbfs against an in-memory filesystem rather than disk-backed storage.

Native `/tmp` result:

| Target | Bandwidth | IOPS | Avg write latency | p50 | p95 | p99 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| native tmpfs | 1580 MiB/s | 405k | 1.03 us | 0.884 us | 1.32 us | 2.544 us |

dbfs on `/tmp` from the same earlier comparison:

| Target | Bandwidth | IOPS | Avg write latency | p50 | p95 | p99 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| dbfs on tmpfs | 28.1 MiB/s | 7,189 | 136.22 us | 104 us | 231 us | 392 us |
