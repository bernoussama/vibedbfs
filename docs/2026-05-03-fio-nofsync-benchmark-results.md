# dbfs fio No-fsync Benchmark Results

Date: 2026-05-03

## Workload

Both runs used the same disk-backed scratch area on the project filesystem:

```text
/home/oussama/projects/dbfs/.fio-disk-nofsync.slHjPW
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
  --fsync=0 \
  --ioengine=sync
```

The dbfs run used:

```bash
scripts/bench-fio.sh \
  --database /home/oussama/projects/dbfs/.fio-disk-nofsync.slHjPW/dbfs.sqlite \
  --mountpoint /home/oussama/projects/dbfs/.fio-disk-nofsync.slHjPW/dbfs-mnt \
  --job randwrite \
  --size 512M \
  --bs 4k \
  --fsync 0
```

## Results

| Target | Runtime | Bandwidth | IOPS | Avg write latency | p50 | p95 | p99 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| dbfs | 49.50s | 10.3 MiB/s | 2,647 | 375.50 us | 151 us | 465 us | 1,385 us |
| native btrfs | 0.928s | 552 MiB/s | 141k | 5.94 us | 6 us | 9 us | 22 us |

For this no-fsync workload, native btrfs showed about 53.6x higher bandwidth and 53.3x higher IOPS than dbfs.

## Notes

Removing fio fsync changes the native result dramatically:

```text
native btrfs with fsync=1: 3.06 MiB/s, 784 IOPS
native btrfs with fsync=0: 552 MiB/s, 141k IOPS
```

dbfs barely changed:

```text
dbfs with fsync=1: 10.4 MiB/s, 2,653 IOPS
dbfs with fsync=0: 10.3 MiB/s, 2,647 IOPS
```

This matches the current implementation: dbfs does not implement FUSE fsync, while each write still performs a SQLite transaction commit. In this benchmark shape, dbfs performance is dominated by per-write SQLite work rather than fio's fsync setting.
