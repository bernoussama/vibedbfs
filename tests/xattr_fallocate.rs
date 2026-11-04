//! `Dbfs`-level behaviour for extended attributes and `fallocate`. The FUSE
//! wrappers are thin, so the errno user space sees on a real mount is checked
//! in `tests/mount_integration.rs`.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dbfs::{Db, Dbfs};

const KEEP_SIZE: i32 = libc::FALLOC_FL_KEEP_SIZE;
const PUNCH_HOLE: i32 = libc::FALLOC_FL_PUNCH_HOLE;
const COLLAPSE_RANGE: i32 = libc::FALLOC_FL_COLLAPSE_RANGE;
const ZERO_RANGE: i32 = libc::FALLOC_FL_ZERO_RANGE;
const INSERT_RANGE: i32 = libc::FALLOC_FL_INSERT_RANGE;
const UNSHARE_RANGE: i32 = libc::FALLOC_FL_UNSHARE_RANGE;

/// A file-system with one empty regular file at `name`, returning its inode.
fn file_fs(name: &[u8]) -> (Dbfs, u64) {
    let db = Db::open_in_memory().expect("open in-memory database");
    let file = db
        .create_file(1, name, 0o644, 1000, 1000)
        .expect("create file");
    (Dbfs::new(db), file.ino)
}

/// Non-zero bytes, so holes (zeros) can't be confused with written data.
fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8 + 1).collect()
}

#[test]
fn getxattr_reports_enodata_for_any_name_on_an_existing_inode() {
    let (fs, file) = file_fs(b"notes.txt");
    let dir = fs
        .mkdir(1, b"docs", 0o755, 0, 1000, 1000)
        .expect("mkdir")
        .ino;

    for ino in [file, dir, 1] {
        for name in [
            &b"user.x"[..],
            b"security.selinux",
            b"trusted.overlay.opaque",
        ] {
            assert_eq!(
                fs.getxattr(ino, name).expect_err("getxattr"),
                libc::ENODATA,
                "inode {ino} name {}",
                String::from_utf8_lossy(name)
            );
        }
    }
}

#[test]
fn listxattr_is_empty_and_setxattr_is_unsupported_on_an_existing_inode() {
    let (fs, file) = file_fs(b"notes.txt");

    assert_eq!(fs.listxattr(file).expect("listxattr"), Vec::<u8>::new());
    assert_eq!(
        fs.setxattr(file, b"user.x").expect_err("setxattr"),
        libc::EOPNOTSUPP
    );
    assert_eq!(
        fs.removexattr(file, b"user.x").expect_err("removexattr"),
        libc::ENODATA
    );
    assert_eq!(
        fs.listxattr(file).expect("listxattr after refused set"),
        Vec::<u8>::new()
    );
}

#[test]
fn xattr_calls_on_a_missing_inode_return_enoent() {
    let (fs, _) = file_fs(b"notes.txt");

    assert_eq!(
        fs.getxattr(9_999, b"user.x").expect_err("getxattr"),
        libc::ENOENT
    );
    assert_eq!(fs.listxattr(9_999).expect_err("listxattr"), libc::ENOENT);
    assert_eq!(
        fs.setxattr(9_999, b"user.x").expect_err("setxattr"),
        libc::ENOENT
    );
    assert_eq!(
        fs.removexattr(9_999, b"user.x").expect_err("removexattr"),
        libc::ENOENT
    );
}

#[test]
fn fallocate_mode_zero_extends_size_and_reads_zeros_past_the_old_end() {
    let (fs, file) = file_fs(b"big.bin");
    fs.write(file, 0, b"head").expect("buffered write");

    fs.fallocate(file, 0, 10_000, 0).expect("fallocate");

    assert_eq!(fs.getattr(file).expect("getattr").size, 10_000);
    let data = fs.read(file, 0, 20_000).expect("read");
    assert_eq!(data.len(), 10_000);
    assert_eq!(&data[..4], b"head");
    assert!(data[4..].iter().all(|&byte| byte == 0));
}

#[test]
fn fallocate_extends_past_multi_chunk_buffered_writes_and_keeps_their_bytes() {
    let (fs, file) = file_fs(b"multi.bin");
    let written = pattern(70_000);
    fs.write(file, 0, &written).expect("buffered write");

    fs.fallocate(file, 0, 200_000, 0).expect("fallocate");

    assert_eq!(fs.getattr(file).expect("getattr").size, 200_000);
    let data = fs.read(file, 0, 200_000).expect("read");
    assert_eq!(data.len(), 200_000);
    assert_eq!(&data[..70_000], &written[..]);
    assert!(data[70_000..].iter().all(|&byte| byte == 0));
}

#[test]
fn fallocate_extends_a_flushed_file_too() {
    let (fs, file) = file_fs(b"flushed.bin");
    fs.write(file, 0, b"abc").expect("buffered write");
    fs.flush_file(file).expect("flush");

    fs.fallocate(file, 100, 50, 0).expect("fallocate");

    assert_eq!(fs.getattr(file).expect("getattr").size, 150);
    let data = fs.read(file, 0, 150).expect("read");
    assert_eq!(&data[..3], b"abc");
    assert!(data[3..].iter().all(|&byte| byte == 0));
}

#[test]
fn fallocate_extension_updates_mtime_and_ctime() {
    let (fs, file) = file_fs(b"notes.txt");
    fs.setattr(
        file,
        None,
        None,
        None,
        None,
        None,
        Some(UNIX_EPOCH + Duration::from_secs(1)),
    )
    .expect("set old mtime");

    let before = SystemTime::now();
    fs.fallocate(file, 0, 100, 0).expect("fallocate");

    let attr = fs.getattr(file).expect("getattr");
    assert!(attr.mtime >= before, "mtime {:?}", attr.mtime);
    assert!(attr.ctime >= before, "ctime {:?}", attr.ctime);
}

#[test]
fn fallocate_inside_the_current_size_leaves_timestamps_alone() {
    let (fs, file) = file_fs(b"notes.txt");
    fs.write(file, 0, b"abcdef").expect("write");
    fs.flush_file(file).expect("flush");
    let before = fs.getattr(file).expect("getattr");

    fs.fallocate(file, 1, 2, 0).expect("fallocate inside size");

    let after = fs.getattr(file).expect("getattr");
    assert_eq!(after.mtime, before.mtime);
    assert_eq!(after.ctime, before.ctime);
}

#[test]
fn fallocate_keep_size_changes_nothing() {
    let (fs, file) = file_fs(b"notes.txt");
    fs.write(file, 0, b"abc").expect("write");

    fs.fallocate(file, 0, 10_000, KEEP_SIZE)
        .expect("keep-size fallocate");

    assert_eq!(fs.getattr(file).expect("getattr").size, 3);
    assert_eq!(fs.read(file, 0, 100).expect("read"), b"abc");
}

#[test]
fn fallocate_rejects_punch_hole_and_range_modes_with_eopnotsupp() {
    let (fs, file) = file_fs(b"notes.txt");
    fs.write(file, 0, b"abc").expect("write");

    for mode in [
        PUNCH_HOLE,
        PUNCH_HOLE | KEEP_SIZE,
        ZERO_RANGE,
        ZERO_RANGE | KEEP_SIZE,
        COLLAPSE_RANGE,
        INSERT_RANGE,
        UNSHARE_RANGE,
        KEEP_SIZE | UNSHARE_RANGE,
        1 << 30,
    ] {
        assert_eq!(
            fs.fallocate(file, 0, 100, mode)
                .expect_err("unsupported mode"),
            libc::EOPNOTSUPP,
            "mode {mode:#x}"
        );
    }
    assert_eq!(fs.getattr(file).expect("getattr").size, 3);
    assert_eq!(fs.read(file, 0, 100).expect("read"), b"abc");
}

#[test]
fn fallocate_rejects_negative_offset_and_non_positive_length_with_einval() {
    let (fs, file) = file_fs(b"notes.txt");

    assert_eq!(fs.fallocate(file, -1, 10, 0), Err(libc::EINVAL));
    assert_eq!(fs.fallocate(file, 0, 0, 0), Err(libc::EINVAL));
    assert_eq!(fs.fallocate(file, 0, -5, 0), Err(libc::EINVAL));
    assert_eq!(fs.fallocate(file, 0, -5, KEEP_SIZE), Err(libc::EINVAL));
    assert_eq!(fs.getattr(file).expect("getattr").size, 0);
}

#[test]
fn fallocate_on_a_missing_inode_is_enoent() {
    let (fs, _) = file_fs(b"notes.txt");

    assert_eq!(fs.fallocate(9_999, 0, 10, 0), Err(libc::ENOENT));
}

#[test]
fn fallocate_on_a_directory_or_symlink_is_enodev() {
    let (fs, _) = file_fs(b"notes.txt");
    let dir = fs
        .mkdir(1, b"docs", 0o755, 0, 1000, 1000)
        .expect("mkdir")
        .ino;
    let link = fs
        .symlink(1, b"link", b"target", 1000, 1000)
        .expect("symlink")
        .ino;

    assert_eq!(fs.fallocate(dir, 0, 10, 0), Err(libc::ENODEV));
    assert_eq!(fs.fallocate(link, 0, 10, 0), Err(libc::ENODEV));
}

#[test]
fn fallocate_past_the_largest_storable_size_returns_efbig() {
    let (fs, file) = file_fs(b"notes.txt");

    assert_eq!(fs.fallocate(file, i64::MAX - 10, 100, 0), Err(libc::EFBIG));
    assert_eq!(fs.fallocate(file, 1, i64::MAX, 0), Err(libc::EFBIG));
    assert_eq!(fs.fallocate(file, 1, i64::MAX, KEEP_SIZE), Err(libc::EFBIG));
    assert_eq!(fs.getattr(file).expect("getattr").size, 0);
}

#[test]
fn fallocate_to_the_largest_storable_size_succeeds() {
    let (fs, file) = file_fs(b"notes.txt");

    // 2^24 chunks of 64 KiB: 1 TiB, the most one file can store.
    const MAX_FILE_SIZE: i64 = 1 << 40;
    fs.fallocate(file, 0, MAX_FILE_SIZE, 0)
        .expect("fallocate to max");
    assert_eq!(
        fs.getattr(file).expect("getattr").size,
        MAX_FILE_SIZE as u64
    );

    assert_eq!(
        fs.fallocate(file, 0, MAX_FILE_SIZE + 1, 0),
        Err(libc::EFBIG)
    );
    assert_eq!(
        fs.fallocate(file, MAX_FILE_SIZE, 1, KEEP_SIZE),
        Err(libc::EFBIG)
    );
    assert_eq!(
        fs.getattr(file).expect("getattr").size,
        MAX_FILE_SIZE as u64
    );
}

#[test]
fn fallocate_never_shrinks_the_file_flushed_or_buffered() {
    let (fs, file) = file_fs(b"notes.txt");
    let written = pattern(100);
    fs.write(file, 0, &written).expect("buffered write");

    fs.fallocate(file, 0, 10, 0)
        .expect("range inside buffered size");
    fs.fallocate(file, 50, 20, 0)
        .expect("range straddling buffered size");
    assert_eq!(fs.getattr(file).expect("getattr").size, 100);
    assert_eq!(fs.read(file, 0, 100).expect("read"), written);

    fs.flush_file(file).expect("flush");
    fs.fallocate(file, 0, 10, 0)
        .expect("range inside flushed size");
    assert_eq!(fs.getattr(file).expect("getattr").size, 100);
    assert_eq!(fs.read(file, 0, 100).expect("read"), written);
}

#[test]
fn fallocate_keeps_the_buffered_bytes_the_extension_sits_next_to() {
    let (fs, file) = file_fs(b"notes.txt");
    fs.write(file, 0, b"tail").expect("buffered write");
    fs.fallocate(file, 0, 8, 0).expect("extend");
    fs.write(file, 6, b"ZZ").expect("write into the extension");

    let data = fs.read(file, 0, 8).expect("read");
    assert_eq!(data, b"tail\0\0ZZ");
}
