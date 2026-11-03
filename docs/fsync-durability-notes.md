# fsync durability measurements

WAL `fdatasync` is the host durability path for `fsync`. `PRAGMA synchronous = FULL` stays off. The two options take the same time on this volume, and `FULL` leaves an empty transaction unsynced.

## Setup

The machine has 4 cores and ext4 on `/dev/vdc`. `rustc` is 1.99.1. SQLite is 3.45.1. Load average before the timed runs was 0.19, 0.25, 0.19. The timed program is a release binary that uses the same pragmas as dbfs. `wal_autocheckpoint` is 10000. Each run opens a new database, inserts one 4 KiB row, then times 80 transactions that each insert another 4 KiB row. Every timed run reported 0 errors and 81 rows.

The options alternate in this order on every round. `fdatasync` commits with `synchronous = NORMAL` and then `fdatasync`s the WAL. `full` sets `synchronous = FULL` before the timed inserts. `fdatasync-both` does the NORMAL commit and then `fdatasync`s the WAL and the main database file.

## Timed runs

Per-operation time in microseconds.

`fdatasync` runs were 187.3, 150.0, 155.9, 175.3, and 129.7. Median 155.9. Range 129.7 to 187.3.

`full` runs were 147.5, 157.0, 139.4, 138.7, and 153.7. Median 147.5. Range 138.7 to 157.0.

`fdatasync-both` runs were 136.1, 145.6, 178.5, 131.6, and 173.4. Median 145.6. Range 131.6 to 178.5.

The gap between the medians is smaller than the run-to-run range. There is no measurable difference.

## Syscalls outside the timed set

A separate 8-commit trace shows where the syncs go.

`full` `fdatasync`s the WAL on each timed commit. It `fdatasync`s the main database file when the connection closes and checkpoints, not during those commits.

`fdatasync` `fdatasync`s the WAL once per timed commit from a second file descriptor. It does not `fdatasync` the main database file during those commits.

`fdatasync-both` also `fdatasync`s the main database file on every commit. `full` does not do that here, so `fsync` does not either while the WAL file exists. When the WAL file is absent, `fsync` `fdatasync`s the main database file.

An earlier Python session on this same SQLite switched an open WAL database to `synchronous = FULL` and committed an empty transaction. The strace log had no `fsync` and no `fdatasync` for that empty commit.

## Limiter

`strace -c` on a 20-commit `full` run, not one of the timed runs, put 35.91% of traced time in `pwrite64` (174 calls) and 23.50% in `fdatasync` (30 calls, about 7 us each). During a separate 40-commit `fdatasync` run, `/proc/diskstats` for `vdc` gained 456 write operations and 3648 sectors. The writes reached the block device. This virtual disk completes `fdatasync` in microseconds, so removing the sync would not cut the commit time in half. The commit is bound by the small WAL writes.

## FUSE check

`strace -p` on the release `dbfs` mount, after `fusermount3` had succeeded, counted 9 successful `fdatasync` calls, all on the WAL file. The client did 8 writes with `fsync` and then one `fsync` with nothing new to write.

A timed FUSE before and after pass ran while another job's fio wrote 512 MiB through a different dbfs mount on this same volume. That pass is discarded. The syncing side waits on the device queue and the old side does not, so the two sides do not see the same noise.
