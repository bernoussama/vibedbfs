//! `PRAGMA mmap_size` on file databases: the default, and the size
//! `Db::set_mmap_size` reports as being in force.

use std::path::PathBuf;

use dbfs::Db;

fn open_fresh(name: &str) -> Db {
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("mmap-size-{name}-{}.sqlite", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(path.with_extension("sqlite-wal"));
    let _ = std::fs::remove_file(path.with_extension("sqlite-shm"));
    Db::open(path).expect("open file database")
}

#[test]
fn file_databases_get_the_default_map() {
    let db = open_fresh("default");

    let size = db.pragma_i64("mmap_size").expect("read mmap_size");

    if cfg!(target_pointer_width = "64") {
        assert!(size >= 1 << 30, "default mmap_size is {size}");
    } else {
        assert_eq!(size, 0);
    }
}

#[test]
fn set_mmap_size_reports_the_size_in_force() {
    let db = open_fresh("set");

    assert_eq!(db.set_mmap_size(1 << 20).expect("set 1 MiB"), 1 << 20);
    assert_eq!(db.pragma_i64("mmap_size").expect("read"), 1 << 20);

    assert_eq!(db.set_mmap_size(0).expect("set 0"), 0);
    assert_eq!(db.pragma_i64("mmap_size").expect("read"), 0);

    // SQLite caps the request at its own maximum. The caller gets the capped value.
    let capped = db.set_mmap_size(1 << 40).expect("set 1 TiB");
    assert!(capped <= 1 << 40, "capped to {capped}");
    assert_eq!(db.pragma_i64("mmap_size").expect("read"), capped as i64);
}
