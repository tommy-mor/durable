//! Runtime: append → reduce → read, pre-tape validation, atomic batch
//! read-your-writes, idempotency, Halted state, backup_tape, typed watch,
//! rebuild, and incremental == replay.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use durable::{
    watch_hit, Durable, Error, Leaf, List, Map, OpenOptions, ProjectionId, Runtime, Tx,
};
use serde::{Deserialize, Serialize};
use tempfile::TempDir;

#[derive(Durable)]
#[allow(dead_code)]
struct Store {
    events: List<Leaf<Evidence>>,
    evidence_by_id: Map<String, Leaf<Evidence>>,
    event_ids_by_kind: Map<String, List<Leaf<String>>>,
    latest_emission: Leaf<Emission>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase")]
enum Event {
    Evidence(Evidence),
    Emission(Emission),
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
struct Evidence {
    id: String,
    kind: String,
    epoch: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
struct Emission {
    epoch: u64,
    ranking: Vec<String>,
}

fn reduce(tx: &mut Tx, event: &Event) -> durable::Result<()> {
    let root = Store::root();
    match event {
        Event::Evidence(e) => {
            if e.id.is_empty() {
                return Err(Error::Reducer("evidence id cannot be empty".into()));
            }
            if root.evidence_by_id().contains(tx, &e.id)? {
                return Err(Error::Reducer(format!("duplicate evidence id {}", e.id)));
            }
            root.events().push(tx, e)?;
            root.evidence_by_id().set(tx, &e.id, e)?;
            root.event_ids_by_kind().key(&e.kind).push(tx, &e.id)?;
        }
        Event::Emission(em) => {
            root.latest_emission().set(tx, em)?;
        }
    }
    Ok(())
}

fn open_rt(dir: &TempDir) -> Runtime<Event> {
    Runtime::open::<Store>(dir.path().join("proj"), dir.path().join("log.jsonl"), reduce).unwrap()
}

#[test]
fn append_indexes_everything_and_reads_from_snapshot() {
    let dir = TempDir::new().unwrap();
    let rt = open_rt(&dir);

    rt.append(Event::Evidence(Evidence {
        id: "e1".into(),
        kind: "llm.judgment".into(),
        epoch: 4,
    }))
    .unwrap();
    rt.append(Event::Evidence(Evidence {
        id: "e2".into(),
        kind: "git.commit".into(),
        epoch: 5,
    }))
    .unwrap();
    rt.append(Event::Emission(Emission {
        epoch: 5,
        ranking: vec!["tommy-mor".into()],
    }))
    .unwrap();

    let r = rt.read().unwrap();
    let root = Store::root();
    assert_eq!(
        root.evidence_by_id().get(&r, "e1").unwrap().unwrap().kind,
        "llm.judgment"
    );
    assert_eq!(
        root.event_ids_by_kind().key("llm.judgment").iter(&r).unwrap(),
        vec!["e1".to_string()]
    );
    assert_eq!(
        root.latest_emission().get(&r).unwrap().unwrap().ranking,
        vec!["tommy-mor".to_string()]
    );
}

#[test]
fn rejected_event_never_touches_tape_and_does_not_brick_runtime() {
    let dir = TempDir::new().unwrap();
    let rt = open_rt(&dir);

    rt.append(Event::Evidence(Evidence {
        id: "e1".into(),
        kind: "llm".into(),
        epoch: 1,
    }))
    .unwrap();

    // Empty id -> rejected before tape write!
    let err = rt
        .append(Event::Evidence(Evidence {
            id: "".into(),
            kind: "llm".into(),
            epoch: 2,
        }))
        .unwrap_err();
    assert!(matches!(err, Error::Reducer(_)), "{err:?}");

    // Duplicate id -> rejected by read-your-writes check before tape write!
    let err = rt
        .append(Event::Evidence(Evidence {
            id: "e1".into(),
            kind: "llm".into(),
            epoch: 2,
        }))
        .unwrap_err();
    assert!(matches!(err, Error::Reducer(_)), "{err:?}");

    assert_eq!(rt.log_len().unwrap(), 1);
    assert_eq!(rt.applied().unwrap(), 1);

    // Subsequent valid append succeeds cleanly!
    let rec = rt
        .append(Event::Evidence(Evidence {
            id: "e2".into(),
            kind: "llm".into(),
            epoch: 3,
        }))
        .unwrap();
    assert_eq!(rec.seq, 1);
    rt.verify().unwrap();
}

#[test]
fn append_batch_has_read_your_writes_and_atomic_rollback_on_rejection() {
    let dir = TempDir::new().unwrap();
    let rt = open_rt(&dir);

    // Second event duplicates the first WITHIN the same batch!
    // Read-your-writes inside the batch's WriteTx catches it and aborts the whole batch.
    let err = rt
        .append_batch(vec![
            Event::Evidence(Evidence {
                id: "dup".into(),
                kind: "k".into(),
                epoch: 1,
            }),
            Event::Evidence(Evidence {
                id: "dup".into(),
                kind: "k".into(),
                epoch: 2,
            }),
        ])
        .unwrap_err();
    assert!(matches!(err, Error::Reducer(_)), "{err:?}");
    assert_eq!(rt.log_len().unwrap(), 0);
    assert_eq!(rt.applied().unwrap(), 0);
    assert!(!Store::root()
        .evidence_by_id()
        .contains(&rt.read().unwrap(), "dup")
        .unwrap());
}

#[test]
fn idempotent_append_deduplicates_and_detects_conflict_across_reopen_and_rebuild() {
    let dir = TempDir::new().unwrap();
    let ev1 = Event::Evidence(Evidence {
        id: "e1".into(),
        kind: "k".into(),
        epoch: 1,
    });
    let ev_other = Event::Evidence(Evidence {
        id: "e99".into(),
        kind: "k".into(),
        epoch: 99,
    });
    {
        let rt = open_rt(&dir);
        let first = rt.append_idempotent("op-1", ev1.clone()).unwrap();
        assert!(!first.is_duplicate());
        assert_eq!(first.records().len(), 1);
        assert_eq!(first.records()[0].seq, 0);

        // Retry with same id and same payload -> Duplicate, no new line on tape.
        let retry = rt.append_idempotent("op-1", ev1.clone()).unwrap();
        assert!(retry.is_duplicate());
        assert_eq!(retry.records()[0].seq, 0);
        assert_eq!(rt.log_len().unwrap(), 1);

        // Same id with different payload -> Conflict!
        let err = rt.append_idempotent("op-1", ev_other.clone()).unwrap_err();
        assert!(matches!(err, Error::Conflict { .. }), "{err:?}");
        assert_eq!(rt.log_len().unwrap(), 1);
    }

    // Reopen AND rebuild: idempotency key survives replay from tape!
    let rt = open_rt(&dir);
    rt.rebuild().unwrap();
    let retry = rt.append_idempotent("op-1", ev1).unwrap();
    assert!(retry.is_duplicate());
    assert_eq!(retry.records()[0].seq, 0);
    assert!(matches!(
        rt.append_idempotent("op-1", ev_other),
        Err(Error::Conflict { .. })
    ));
    rt.verify().unwrap();
}

#[test]
fn replay_reducer_error_halts_runtime_until_fixed() {
    let dir = TempDir::new().unwrap();
    {
        let rt = open_rt(&dir);
        rt.append(Event::Evidence(Evidence {
            id: "e1".into(),
            kind: "ok".into(),
            epoch: 1,
        }))
        .unwrap();
        rt.append(Event::Evidence(Evidence {
            id: "poison-on-v2".into(),
            kind: "ok".into(),
            epoch: 2,
        }))
        .unwrap();
    }
    // Reopen with a stricter v2 reducer that rejects seq 1 during replay.
    let err = Runtime::open_with(
        dir.path().join("proj"),
        dir.path().join("log.jsonl"),
        OpenOptions::new::<Store>(ProjectionId::new(2), |tx, ev: &Event| {
            if let Event::Evidence(e) = ev {
                if e.id == "poison-on-v2" {
                    return Err(Error::Reducer("v2 dislikes this".into()));
                }
            }
            reduce(tx, ev)
        }),
    )
    .err()
    .expect("must halt on replay error");
    assert!(matches!(err, Error::Halted { seq: 1, .. }), "{err:?}");

    // Fix the reducer and reopen: catches up cleanly!
    let rt = Runtime::open_with(
        dir.path().join("proj"),
        dir.path().join("log.jsonl"),
        OpenOptions::new::<Store>(ProjectionId::new(2), reduce),
    )
    .unwrap();
    assert_eq!(rt.applied().unwrap(), 2);
    rt.verify().unwrap();
}

#[test]
fn typed_watch_fires_only_on_overlapping_commits() {
    let dir = TempDir::new().unwrap();
    let rt = open_rt(&dir);

    let watch_e1 = Store::root().evidence_by_id().key("e1").watch_range();
    let hits = Arc::new(AtomicUsize::new(0));
    let h = hits.clone();
    rt.on_commit(move |_seq, touched| {
        if watch_hit(&watch_e1, touched) {
            h.fetch_add(1, Ordering::SeqCst);
        }
    });

    rt.append(Event::Evidence(Evidence {
        id: "e2".into(),
        kind: "k".into(),
        epoch: 1,
    }))
    .unwrap();
    assert_eq!(hits.load(Ordering::SeqCst), 0);

    rt.append(Event::Evidence(Evidence {
        id: "e1".into(),
        kind: "k".into(),
        epoch: 2,
    }))
    .unwrap();
    assert_eq!(hits.load(Ordering::SeqCst), 1);

    rt.append(Event::Emission(Emission {
        epoch: 2,
        ranking: vec![],
    }))
    .unwrap();
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

#[test]
fn backup_tape_copies_exact_committed_prefix() {
    let dir = TempDir::new().unwrap();
    let rt = open_rt(&dir);
    for i in 0..5 {
        rt.append(Event::Evidence(Evidence {
            id: format!("e{i}"),
            kind: "k".into(),
            epoch: i,
        }))
        .unwrap();
    }
    let backup = dir.path().join("backup/tape.jsonl");
    let copied = rt.backup_tape(&backup).unwrap();
    assert_eq!(copied, rt.committed_end().unwrap());
    assert_eq!(
        std::fs::read(&backup).unwrap(),
        std::fs::read(dir.path().join("log.jsonl")).unwrap()
    );
    assert_eq!(durable::census::verify(&backup).unwrap().records, 5);
}

#[test]
fn incremental_equals_replay_from_zero_and_fast_catch_up() {
    let dir = TempDir::new().unwrap();
    let rt = open_rt(&dir);
    for i in 0..25 {
        rt.append(Event::Evidence(Evidence {
            id: format!("e{i}"),
            kind: if i % 2 == 0 {
                "llm.judgment".into()
            } else {
                "git.commit".into()
            },
            epoch: i,
        }))
        .unwrap();
    }
    rt.verify().unwrap();
    rt.rebuild().unwrap();
    rt.verify().unwrap();
    assert_eq!(
        Store::root().events().len(&rt.read().unwrap()).unwrap(),
        25
    );
}

#[test]
fn concurrent_snapshots_never_block_on_appends() {
    use std::thread;

    let dir = TempDir::new().unwrap();
    let rt = Arc::new(open_rt(&dir));
    rt.append(Event::Evidence(Evidence {
        id: "seed".into(),
        kind: "llm.judgment".into(),
        epoch: 0,
    }))
    .unwrap();

    let readers: Vec<_> = (0..4)
        .map(|_| {
            let rt = rt.clone();
            thread::spawn(move || {
                let root = Store::root();
                for _ in 0..80 {
                    let r = rt.read().unwrap();
                    assert!(root.evidence_by_id().contains(&r, "seed").unwrap());
                    // Snapshot consistency: `events.len()` == `evidence_by_id.len()`
                    assert_eq!(
                        root.events().len(&r).unwrap(),
                        root.evidence_by_id().len(&r).unwrap()
                    );
                }
            })
        })
        .collect();
    for i in 0..40 {
        rt.append(Event::Evidence(Evidence {
            id: format!("e{i}"),
            kind: "git.commit".into(),
            epoch: i,
        }))
        .unwrap();
    }
    for h in readers {
        h.join().unwrap();
    }
    assert_eq!(rt.applied().unwrap(), 41);
}

#[test]
fn runtime_is_sync() {
    fn assert_sync<T: Sync>() {}
    assert_sync::<Runtime<Event>>();
}
