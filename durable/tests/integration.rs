//! End-to-end tests for the typed `redb`-native durable API.

use durable::{Db, Deque, Durability, Durable, Leaf, List, Map, Set, Sum};
use serde::{Deserialize, Serialize};
use tempfile::TempDir;

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
struct Vote {
    a: String,
    b: String,
    ratio: i32,
}

#[derive(Durable)]
#[allow(dead_code)]
struct GroupState {
    edges: Map<(u32, u32), Sum<f64>>,
    voted_pairs: Set<(u32, u32)>,
    recent_votes: Deque<Leaf<Vote>>,
    item_count: Sum<i64>,
}

#[derive(Durable)]
#[allow(dead_code)]
struct Store {
    scopes: Map<String, GroupState>,
    nodes: Map<String, Leaf<String>>,
    tags: Set<String>,
    log: List<Leaf<u64>>,
    title: Leaf<String>,
    counter: Sum<i64>,
}

fn open() -> (TempDir, Db) {
    let dir = TempDir::new().unwrap();
    let db = Db::open(dir.path()).unwrap();
    (dir, db)
}

#[test]
fn leaf_set_get_update_delete() {
    let (_dir, db) = open();
    let root = Store::root();

    assert_eq!(root.nodes().get(&db, "reddit.com/r/rust").unwrap(), None);
    db.txn(Durability::SyncWal, |tx| {
        root.nodes()
            .set(tx, "reddit.com/r/rust", &"Rust".to_string())?;
        root.title().set(tx, &"home".to_string())?;
        Ok(())
    })
    .unwrap();

    let r = db.read().unwrap();
    assert_eq!(
        root.nodes().get(&r, "reddit.com/r/rust").unwrap(),
        Some("Rust".to_string())
    );
    assert_eq!(root.title().get(&r).unwrap(), Some("home".to_string()));

    db.txn(Durability::SyncWal, |tx| {
        root.nodes().key("reddit.com/r/rust").update(tx, |cur| {
            assert_eq!(cur, Some("Rust".to_string()));
            Some("Rust 2026".to_string())
        })?;
        Ok(())
    })
    .unwrap();
    assert_eq!(
        root.nodes().get(&db, "reddit.com/r/rust").unwrap(),
        Some("Rust 2026".to_string())
    );

    db.txn(Durability::SyncWal, |tx| {
        assert!(root.nodes().key("reddit.com/r/rust").delete(tx)?);
        assert!(!root.nodes().key("reddit.com/r/rust").delete(tx)?);
        Ok(())
    })
    .unwrap();
    assert_eq!(root.nodes().get(&db, "reddit.com/r/rust").unwrap(), None);
}

#[test]
fn sum_accumulates_and_reads_own_writes_in_tx() {
    let (_dir, db) = open();
    let edges = Store::root().scopes().key("s").edges();

    db.txn(Durability::SyncWal, |tx| {
        assert_eq!(edges.add(tx, (3, 7), 2.0)?, 2.0);
        assert_eq!(edges.add(tx, (3, 7), 1.0)?, 3.0);
        assert_eq!(edges.add(tx, (3, 7), 0.5)?, 3.5);
        assert_eq!(edges.get(tx, (3, 7))?, 3.5);
        Ok(())
    })
    .unwrap();
    assert_eq!(edges.get(&db, (3, 7)).unwrap(), 3.5);

    db.txn(Durability::SyncWal, |tx| {
        edges.add(tx, (3, 7), -1.5)?;
        Ok(())
    })
    .unwrap();
    assert_eq!(edges.get(&db, (3, 7)).unwrap(), 2.0);
    assert_eq!(edges.get(&db, (9, 9)).unwrap(), 0.0);
}

#[test]
fn sum_set_then_add() {
    let (_dir, db) = open();
    let count = Store::root().scopes().key("s").item_count();
    db.txn(Durability::SyncWal, |tx| {
        count.set(tx, 10)?;
        assert_eq!(count.add(tx, 5)?, 15);
        Ok(())
    })
    .unwrap();
    assert_eq!(count.get(&db).unwrap(), 15);
}

#[test]
fn point_update_touches_only_its_own_key() {
    let (_dir, db) = open();
    let root = Store::root();
    let rust = root.scopes().key("rust");
    let python = root.scopes().key("python");

    db.txn(Durability::SyncWal, |tx| {
        for j in 0..5u32 {
            rust.edges().add(tx, (0, j), j as f64 + 1.0)?;
            python.edges().add(tx, (0, j), 100.0)?;
        }
        rust.recent_votes().push_back(
            tx,
            &Vote {
                a: "a".into(),
                b: "b".into(),
                ratio: 2,
            },
        )?;
        Ok(())
    })
    .unwrap();

    db.txn(Durability::SyncWal, |tx| {
        rust.edges().add(tx, (0, 2), 10.0)?;
        Ok(())
    })
    .unwrap();

    let r = db.read().unwrap();
    assert_eq!(rust.edges().get(&r, (0, 2)).unwrap(), 13.0);
    assert_eq!(rust.edges().get(&r, (0, 0)).unwrap(), 1.0);
    assert_eq!(rust.edges().get(&r, (0, 4)).unwrap(), 5.0);
    for j in 0..5u32 {
        assert_eq!(python.edges().get(&r, (0, j)).unwrap(), 100.0);
    }
    assert_eq!(rust.recent_votes().len(&r).unwrap(), 1);
}

#[test]
fn map_keys_len_contains_entries_in_lexicographic_order() {
    let (_dir, db) = open();
    let nodes = Store::root().nodes();
    db.txn(Durability::SyncWal, |tx| {
        nodes.set(tx, "zz", &"4".to_string())?;
        nodes.set(tx, "aaa", &"1".to_string())?;
        nodes.set(tx, "b", &"2".to_string())?;
        nodes.set(tx, "c", &"3".to_string())?;
        Ok(())
    })
    .unwrap();

    let r = db.read().unwrap();
    // Lexicographic order — NOT length-first!
    assert_eq!(
        nodes.keys(&r).unwrap(),
        vec!["aaa", "b", "c", "zz"]
    );
    assert_eq!(nodes.len(&r).unwrap(), 4);
    assert!(nodes.contains(&r, "b").unwrap());
    assert!(!nodes.contains(&r, "z").unwrap());
    assert_eq!(
        nodes.iter(&r).unwrap(),
        vec![
            ("aaa".into(), "1".into()),
            ("b".into(), "2".into()),
            ("c".into(), "3".into()),
            ("zz".into(), "4".into()),
        ]
    );
}

#[test]
fn map_keys_dedup_and_remove_nested_struct() {
    let (_dir, db) = open();
    let scopes = Store::root().scopes();
    let rust = scopes.key("rust");

    db.txn(Durability::SyncWal, |tx| {
        rust.edges().add(tx, (0, 1), 1.0)?;
        rust.edges().add(tx, (0, 2), 1.0)?;
        rust.item_count().add(tx, 3)?;
        rust.recent_votes().push_back(
            tx,
            &Vote {
                a: "a".into(),
                b: "b".into(),
                ratio: 1,
            },
        )?;
        scopes.key("python").item_count().add(tx, 1)?;
        Ok(())
    })
    .unwrap();

    let r = db.read().unwrap();
    assert_eq!(scopes.keys(&r).unwrap(), vec!["python", "rust"]);
    assert_eq!(scopes.len(&r).unwrap(), 2);

    // Typed `Map::remove` deletes the entire nested struct subtree!
    db.txn(Durability::SyncWal, |tx| scopes.remove(tx, "rust"))
        .unwrap();
    let r2 = db.read().unwrap();
    assert_eq!(scopes.keys(&r2).unwrap(), vec!["python"]);
    assert_eq!(rust.edges().len(&r2).unwrap(), 0);
    assert_eq!(rust.item_count().get(&r2).unwrap(), 0);
    assert!(rust.recent_votes().is_empty(&r2).unwrap());
    // Original snapshot `r` STILL sees "rust" (MVCC isolation)!
    assert_eq!(scopes.keys(&r).unwrap(), vec!["python", "rust"]);
}

#[test]
fn clear_then_write_in_same_tx_preserves_arrival_order() {
    let (_dir, db) = open();
    let root = Store::root();

    db.txn(Durability::SyncWal, |tx| {
        root.nodes().set(tx, "old", &"1".to_string())?;
        root.log().push(tx, &10)?;
        root.log().push(tx, &20)?;
        Ok(())
    })
    .unwrap();

    // Clear THEN set / push in the SAME transaction — arrival order is honored!
    db.txn(Durability::SyncWal, |tx| {
        root.nodes().clear(tx)?;
        root.nodes().set(tx, "new", &"2".to_string())?;

        root.log().clear(tx)?;
        assert_eq!(root.log().push(tx, &99)?, 0);
        Ok(())
    })
    .unwrap();

    let r = db.read().unwrap();
    assert_eq!(
        root.nodes().iter(&r).unwrap(),
        vec![("new".to_string(), "2".to_string())]
    );
    assert_eq!(root.log().len(&r).unwrap(), 1);
    assert_eq!(root.log().iter(&r).unwrap(), vec![99]);
}

#[test]
fn set_operations_and_o1_len() {
    let (_dir, db) = open();
    let tags = Store::root().tags();

    db.txn(Durability::SyncWal, |tx| {
        tags.insert(tx, "rust")?;
        tags.insert(tx, "redb")?;
        tags.insert(tx, "durable")?;
        tags.insert(tx, "rust")?; // idempotent
        Ok(())
    })
    .unwrap();

    let r = db.read().unwrap();
    assert_eq!(tags.len(&r).unwrap(), 3);
    assert!(tags.contains(&r, "rust").unwrap());
    assert_eq!(tags.first(&r).unwrap(), Some("durable".to_string()));
    assert_eq!(tags.last(&r).unwrap(), Some("rust".to_string()));
    assert_eq!(tags.iter(&r).unwrap(), vec!["durable", "redb", "rust"]);

    db.txn(Durability::SyncWal, |tx| {
        assert!(tags.remove(tx, "redb")?);
        assert!(!tags.remove(tx, "missing")?);
        Ok(())
    })
    .unwrap();
    assert_eq!(tags.len(&db).unwrap(), 2);
}

#[test]
fn transform_values_decays_all_edges_in_one_tx() {
    let (_dir, db) = open();
    let edges = Store::root().scopes().key("rust").edges();

    db.txn(Durability::SyncWal, |tx| {
        for j in 1..=4u32 {
            edges.add(tx, (0, j), j as f64 * 10.0)?;
        }
        Ok(())
    })
    .unwrap();

    db.txn(Durability::SyncWal, |tx| {
        edges.transform_values(tx, |_k, w| {
            let decayed = w * 0.5;
            if decayed <= 5.0 {
                None
            } else {
                Some(decayed)
            }
        })
    })
    .unwrap();

    let r = db.read().unwrap();
    assert_eq!(edges.get(&r, (0, 1)).unwrap(), 0.0);
    assert_eq!(edges.get(&r, (0, 2)).unwrap(), 10.0);
    assert_eq!(edges.get(&r, (0, 3)).unwrap(), 15.0);
    assert_eq!(edges.get(&r, (0, 4)).unwrap(), 20.0);
    assert_eq!(edges.len(&r).unwrap(), 3);
}

#[test]
fn list_push_pop_range_iter() {
    let (_dir, db) = open();
    let log = Store::root().log();

    assert!(log.is_empty(&db).unwrap());
    db.txn(Durability::SyncWal, |tx| {
        assert_eq!(log.push(tx, &10)?, 0);
        assert_eq!(log.push(tx, &20)?, 1);
        assert_eq!(log.push(tx, &30)?, 2);
        assert_eq!(log.push(tx, &40)?, 3);
        Ok(())
    })
    .unwrap();

    let r = db.read().unwrap();
    assert_eq!(log.len(&r).unwrap(), 4);
    assert_eq!(log.get(&r, 1).unwrap(), Some(20));
    assert_eq!(log.get(&r, 4).unwrap(), None);
    assert_eq!(log.first(&r).unwrap(), Some(10));
    assert_eq!(log.last(&r).unwrap(), Some(40));
    assert_eq!(log.iter(&r).unwrap(), vec![10, 20, 30, 40]);
    let slice: Vec<u64> = log.range(&r, 1..3).unwrap().collect::<Result<_, _>>().unwrap();
    assert_eq!(slice, vec![20, 30]);
    let rev: Vec<u64> = log.range(&r, ..).unwrap().rev().take(2).collect::<Result<_, _>>().unwrap();
    assert_eq!(rev, vec![40, 30]);

    let popped = db
        .txn(Durability::SyncWal, |tx| log.pop(tx))
        .unwrap();
    assert_eq!(popped, Some(40));
    assert_eq!(log.len(&db).unwrap(), 3);
}

#[test]
fn deque_transactional_ends_and_truncate() {
    let (_dir, db) = open();
    let dq = Store::root().scopes().key("s").recent_votes();
    let v = |n: i32| Vote {
        a: format!("a{n}"),
        b: format!("b{n}"),
        ratio: n,
    };

    // Mixed front & back pushes in one transaction
    db.txn(Durability::SyncWal, |tx| {
        dq.push_back(tx, &v(1))?;
        dq.push_front(tx, &v(0))?;
        dq.push_back(tx, &v(2))?;
        dq.push_front(tx, &v(-1))?;
        Ok(())
    })
    .unwrap();

    let r = db.read().unwrap();
    assert_eq!(dq.len(&r).unwrap(), 4);
    assert_eq!(dq.front(&r).unwrap(), Some(v(-1)));
    assert_eq!(dq.back(&r).unwrap(), Some(v(2)));
    assert_eq!(dq.iter(&r).unwrap(), vec![v(-1), v(0), v(1), v(2)]);

    // Capped window inside one transaction via truncate_front
    db.txn(Durability::SyncWal, |tx| {
        for n in 3..10 {
            dq.push_back(tx, &v(n))?;
        }
        dq.truncate_front(tx, 3)?;
        Ok(())
    })
    .unwrap();
    let kept: Vec<i32> = dq.iter(&db).unwrap().into_iter().map(|x| x.ratio).collect();
    assert_eq!(kept, vec![7, 8, 9]);

    db.txn(Durability::SyncWal, |tx| dq.truncate_back(tx, 1))
        .unwrap();
    assert_eq!(dq.iter(&db).unwrap(), vec![v(7)]);
}

#[test]
fn namespaced_roots_do_not_collide_and_clear_independently() {
    let db = Db::open_memory().unwrap();
    let a = Store::namespaced("a");
    let b = Store::namespaced("b");

    db.txn(Durability::SyncWal, |tx| {
        a.nodes().set(tx, "k", &"av".to_string())?;
        a.title().set(tx, &"title-a".to_string())?;
        b.nodes().set(tx, "k", &"bv".to_string())?;
        b.title().set(tx, &"title-b".to_string())?;
        Ok(())
    })
    .unwrap();

    let r = db.read().unwrap();
    assert_eq!(a.nodes().get(&r, "k").unwrap(), Some("av".to_string()));
    assert_eq!(b.nodes().get(&r, "k").unwrap(), Some("bv".to_string()));
    assert_eq!(a.title().get(&r).unwrap(), Some("title-a".to_string()));
    assert_eq!(b.title().get(&r).unwrap(), Some("title-b".to_string()));

    db.txn(Durability::SyncWal, |tx| a.clear(tx)).unwrap();
    let r2 = db.read().unwrap();
    assert_eq!(a.nodes().get(&r2, "k").unwrap(), None);
    assert_eq!(a.title().get(&r2).unwrap(), None);
    assert_eq!(b.nodes().get(&r2, "k").unwrap(), Some("bv".to_string()));
    assert_eq!(b.title().get(&r2).unwrap(), Some("title-b".to_string()));
}

#[test]
fn persistence_across_reopen_for_all_collection_kinds() {
    let dir = TempDir::new().unwrap();
    {
        let db = Db::open(dir.path()).unwrap();
        let root = Store::root();
        let s = root.scopes().key("s");
        db.txn(Durability::SyncWal, |tx| {
            root.nodes().set(tx, "n", &"N".to_string())?;
            root.tags().insert(tx, "t1")?;
            root.log().push(tx, &42)?;
            root.title().set(tx, &"hello".to_string())?;
            root.counter().add(tx, 7)?;
            s.edges().add(tx, (1, 2), 9.0)?;
            s.voted_pairs().insert(tx, (1, 2))?;
            s.recent_votes().push_back(
                tx,
                &Vote {
                    a: "a".into(),
                    b: "b".into(),
                    ratio: 3,
                },
            )?;
            Ok(())
        })
        .unwrap();
    }
    let db = Db::open(dir.path()).unwrap();
    let r = db.read().unwrap();
    let root = Store::root();
    let s = root.scopes().key("s");
    assert_eq!(root.nodes().get(&r, "n").unwrap(), Some("N".to_string()));
    assert!(root.tags().contains(&r, "t1").unwrap());
    assert_eq!(root.log().iter(&r).unwrap(), vec![42]);
    assert_eq!(root.title().get(&r).unwrap(), Some("hello".to_string()));
    assert_eq!(root.counter().get(&r).unwrap(), 7);
    assert_eq!(s.edges().get(&r, (1, 2)).unwrap(), 9.0);
    assert!(s.voted_pairs().contains(&r, (1, 2)).unwrap());
    assert_eq!(
        s.recent_votes().front(&r).unwrap(),
        Some(Vote {
            a: "a".into(),
            b: "b".into(),
            ratio: 3,
        })
    );
}
