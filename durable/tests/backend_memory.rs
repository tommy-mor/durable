//! In-memory backend coverage: ephemeral, path-agnostic, and shared by
//! clones — the test-only engine with no disk I/O.
//!
//! Compiles only with `--no-default-features --features backend-memory`.

#![cfg(feature = "backend-memory")]

use durable::{Db, Durability, Durable, Leaf, List, Map, Sum};

#[derive(Durable)]
#[allow(dead_code)]
struct Store {
    scores: Map<String, Sum<i64>>,
    title: Leaf<String>,
    log: List<Leaf<u64>>,
}

#[test]
fn open_memory_is_empty_and_roundtrips() {
    let db = Db::open_memory().unwrap();
    let root = Store::root();
    assert_eq!(root.title().get(&db).unwrap(), None);

    db.run(root.title().set(&"hello".to_string()), Durability::SyncWal)
        .unwrap();
    db.apply(
        &[
            root.scores().key(&"alice".to_string()).add(10),
            root.scores().key(&"alice".to_string()).add(5),
        ],
        Durability::WalOnly,
    )
    .unwrap();
    root.log().push(&db, &42).unwrap();

    assert_eq!(root.title().get(&db).unwrap(), Some("hello".to_string()));
    assert_eq!(root.scores().key(&"alice".to_string()).get(&db).unwrap(), 15);
    assert_eq!(root.log().iter(&db).unwrap(), vec![42]);
}

#[test]
fn clones_share_the_same_map() {
    let db = Db::open_memory().unwrap();
    let cloned = db.clone();
    let root = Store::root();

    db.run(root.title().set(&"shared".to_string()), Durability::SyncWal)
        .unwrap();
    // The clone sees the write without a reopen — same `Arc<Store>` map.
    assert_eq!(root.title().get(&cloned).unwrap(), Some("shared".to_string()));

    cloned
        .run(root.title().set(&"updated".to_string()), Durability::DisableWal)
        .unwrap();
    assert_eq!(root.title().get(&db).unwrap(), Some("updated".to_string()));
}

#[test]
fn memory_is_ephemeral_across_instances() {
    let root = Store::root();
    let first = Db::open_memory().unwrap();
    first
        .run(root.title().set(&"gone".to_string()), Durability::SyncWal)
        .unwrap();
    drop(first);

    // A fresh instance starts empty — nothing was written to disk.
    let fresh = Db::open_memory().unwrap();
    assert_eq!(root.title().get(&fresh).unwrap(), None);
}

#[test]
fn open_with_a_path_ignores_the_path() {
    // Genuine temp paths must not scatter files for the memory backend,
    // and each `open` is independent. We only need one `TempDir` to pass;
    // the store never touches it.
    let dir = tempfile::TempDir::new().unwrap();
    let db = Db::open(dir.path()).unwrap();
    let root = Store::root();

    db.run(root.title().set(&"ephemeral".to_string()), Durability::SyncWal)
        .unwrap();
    assert_eq!(root.title().get(&db).unwrap(), Some("ephemeral".to_string()));

    // No backend file was created inside the supplied directory.
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);

    // A second `open` on the same path is a *different* ephemeral store.
    let other = Db::open(dir.path()).unwrap();
    assert_eq!(root.title().get(&other).unwrap(), None);
}

#[test]
fn durability_levels_are_all_accepted() {
    let db = Db::open_memory().unwrap();
    let root = Store::root();
    for durability in [Durability::SyncWal, Durability::WalOnly, Durability::DisableWal] {
        db.run(root.title().set(&"v".to_string()), durability).unwrap();
    }
    assert_eq!(root.title().get(&db).unwrap(), Some("v".to_string()));
}
