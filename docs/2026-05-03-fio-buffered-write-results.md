# dbfs Buffered-write fio Results

Date: 2026-05-03

## Change

dbfs now buffers FUSE writes in memory and flushes dirty file data to SQLite on `flush`, `fsync`, and `release`.

Before this change, each FUSE write called `Db::write_file` immediately, which meant every 4 KiB fio write became its own SQLite transaction.

## Workload

Both updated dbfs runs used disk-backed scratch directories on:

```text
/dev/nvme0n1p6 btrfs
```

fio profile:

```bash
fio --name=dbfs-randwrite \
  --directory=<dbfs-mountpoint> \
  --size=512M \
  --bs=4k \
  --rw=randwrite \
  --ioengine=sync \
  --fsync=<0-or-1>
```

## Updated Results

| Target | fsync | Runtime | Bandwidth | IOPS | Avg write latency | p50 | p95 | p99 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| dbfs buffered writes | 0 | 3.17s | 161 MiB/s | 41.3k | 23.08 us | 21 us | 35 us | 59 us |
| dbfs buffered writes | 1 | 51.80s | 9.88 MiB/s | 2,530 | 37.30 us | 29 us | 77 us | 114 us |

## Before and After

No-fsync dbfs before buffering:

```text
10.3 MiB/s, 2,647 IOPS, 49.50s
```

No-fsync dbfs after buffering:

```text
161 MiB/s, 41.3k IOPS, 3.17s
```

That is about 15.6x higher bandwidth and 15.6x higher IOPS for the no-fsync workload.

## Notes

The fsync-heavy run remains slow because `fio --fsync=1` forces dbfs to flush after every 4 KiB write. That intentionally removes most of the benefit of write buffering.

The current `fsync` implementation flushes dbfs dirty writes into SQLite. It does not yet change SQLite from `PRAGMA synchronous = NORMAL` to a stronger durability mode.
