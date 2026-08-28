//! Runtime: append → reduce → query, rebuild, incremental == replay.

use ciborium::Value;
use durable::{Durable, Leaf, List, Map, Nav, Query, Runtime, Tx};
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
            tx.write(root.events().push_op(e));
            tx.write(root.evidence_by_id().key(&e.id).set(e));
            tx.write(root.event_ids_by_kind().key(&e.kind).push_op(&e.id));
        }
        Event::Emission(em) => {
            tx.write(root.latest_emission().set(em));
        }
    }
    Ok(())
}

fn open_rt(dir: &TempDir) -> Runtime<Event> {
    Runtime::open_described::<Store>(
        dir.path().join("proj"),
        dir.path().join("log.jsonl"),
        None,
        reduce,
    )
    .unwrap()
}

#[test]
fn append_indexes_everything() {
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

    let kind = rt
        .one(&Query::new(vec![
            Nav::Field("evidence_by_id".into()),
            Nav::Key(Value::Text("e1".into())),
            Nav::Field("kind".into()),
        ]))
        .unwrap()
        .unwrap();
    assert!(matches!(kind, Value::Text(s) if s == "llm.judgment"));

    let ids = rt
        .select(&Query::new(vec![
            Nav::Field("event_ids_by_kind".into()),
            Nav::Key(Value::Text("llm.judgment".into())),
            Nav::All,
        ]))
        .unwrap();
    assert_eq!(ids.len(), 1);

    let latest = rt
        .one(&Query::new(vec![Nav::Field("latest_emission".into())]))
        .unwrap()
        .unwrap();
    match latest {
        Value::Map(pairs) => {
            let ranking = pairs
                .iter()
                .find(|(k, _)| matches!(k, Value::Text(s) if s == "ranking"))
                .map(|(_, v)| v)
                .unwrap();
            match ranking {
                Value::Array(xs) => assert_eq!(xs.len(), 1),
                other => panic!("{other:?}"),
            }
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn incremental_equals_replay_from_zero() {
    let dir = TempDir::new().unwrap();
    let rt = open_rt(&dir);
    for i in 0..20 {
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

    let kinds = rt
        .select(&Query::new(vec![
            Nav::Field("events".into()),
            Nav::All,
            Nav::Field("kind".into()),
        ]))
        .unwrap();
    assert_eq!(kinds.len(), 20);
}

#[test]
fn catch_up_from_existing_log() {
    let dir = TempDir::new().unwrap();
    {
        let rt = open_rt(&dir);
        rt.append(Event::Evidence(Evidence {
            id: "e1".into(),
            kind: "llm.judgment".into(),
            epoch: 1,
        }))
        .unwrap();
    }
    // Reopen: projection exists and should catch up (no-op) then accept more.
    let rt = open_rt(&dir);
    assert_eq!(rt.applied().unwrap(), 1);
    rt.append(Event::Evidence(Evidence {
        id: "e2".into(),
        kind: "git.commit".into(),
        epoch: 2,
    }))
    .unwrap();
    assert_eq!(rt.applied().unwrap(), 2);
    rt.verify().unwrap();
}

#[test]
fn append_keeps_log_len_without_reread() {
    let dir = TempDir::new().unwrap();
    let rt = open_rt(&dir);
    for i in 0..50 {
        rt.append(Event::Evidence(Evidence {
            id: format!("e{i}"),
            kind: "llm.judgment".into(),
            epoch: i,
        }))
        .unwrap();
    }
    assert_eq!(rt.log_len().unwrap(), 50);
    assert_eq!(rt.applied().unwrap(), 50);
    rt.verify().unwrap();
}

#[test]
fn ingest_stamps_seq_and_monotonic_ts() {
    let dir = TempDir::new().unwrap();
    let rt = open_rt(&dir);
    let a = rt
        .append(Event::Evidence(Evidence {
            id: "e1".into(),
            kind: "llm.judgment".into(),
            epoch: 1,
        }))
        .unwrap();
    let b = rt
        .append(Event::Evidence(Evidence {
            id: "e2".into(),
            kind: "git.commit".into(),
            epoch: 2,
        }))
        .unwrap();
    assert_eq!(a.seq, 0);
    assert_eq!(b.seq, 1);
    assert!(b.ts_ms >= a.ts_ms);
    assert!(a.ts_ms > 0);

    let tape = std::fs::read_to_string(dir.path().join("log.jsonl")).unwrap();
    let line: serde_json::Value = serde_json::from_str(tape.lines().next().unwrap()).unwrap();
    assert_eq!(line["seq"], 0);
    assert!(line["ts_ms"].as_u64().unwrap() > 0);
    assert_eq!(line["event"]["type"], "evidence");
    assert!(
        line.get("type").is_none(),
        "event body is nested, not flattened"
    );
}

#[test]
fn append_batch_is_one_contiguous_seq_run() {
    let dir = TempDir::new().unwrap();
    let rt = open_rt(&dir);
    let recs = rt
        .append_batch(vec![
            Event::Evidence(Evidence {
                id: "e0".into(),
                kind: "llm.judgment".into(),
                epoch: 0,
            }),
            Event::Evidence(Evidence {
                id: "e1".into(),
                kind: "git.commit".into(),
                epoch: 1,
            }),
            Event::Evidence(Evidence {
                id: "e2".into(),
                kind: "llm.judgment".into(),
                epoch: 2,
            }),
        ])
        .unwrap();
    assert_eq!(recs.len(), 3);
    assert_eq!(recs[0].seq, 0);
    assert_eq!(recs[1].seq, 1);
    assert_eq!(recs[2].seq, 2);
    assert!(recs[0].ts_ms <= recs[1].ts_ms && recs[1].ts_ms <= recs[2].ts_ms);
    assert_eq!(rt.log_len().unwrap(), 3);
    assert_eq!(rt.applied().unwrap(), 3);

    let kinds = rt
        .select(&Query::new(vec![
            Nav::Field("events".into()),
            Nav::All,
            Nav::Field("kind".into()),
        ]))
        .unwrap();
    assert_eq!(kinds.len(), 3);
    rt.verify().unwrap();
}

#[test]
fn reducer_sees_ingest_meta() {
    use durable::Tx;
    use std::sync::{Mutex, OnceLock};

    static LAST: OnceLock<Mutex<Option<(u64, u64)>>> = OnceLock::new();
    fn last() -> &'static Mutex<Option<(u64, u64)>> {
        LAST.get_or_init(|| Mutex::new(None))
    }

    fn reduce_meta(tx: &mut Tx, event: &Event) -> durable::Result<()> {
        *last().lock().unwrap() = Some((tx.meta().seq, tx.meta().ts_ms));
        reduce(tx, event)
    }

    let dir = TempDir::new().unwrap();
    let rt = Runtime::open_described::<Store>(
        dir.path().join("proj"),
        dir.path().join("log.jsonl"),
        None,
        reduce_meta,
    )
    .unwrap();
    let rec = rt
        .append(Event::Evidence(Evidence {
            id: "e1".into(),
            kind: "llm.judgment".into(),
            epoch: 1,
        }))
        .unwrap();
    let seen = last().lock().unwrap().unwrap();
    assert_eq!(seen.0, rec.seq);
    assert_eq!(seen.1, rec.ts_ms);
}

#[test]
fn queries_run_without_exclusive_lock() {
    use std::sync::Arc;
    use std::thread;

    let dir = TempDir::new().unwrap();
    let rt = Arc::new(open_rt(&dir));
    rt.append(Event::Evidence(Evidence {
        id: "seed".into(),
        kind: "llm.judgment".into(),
        epoch: 0,
    }))
    .unwrap();

    let q = Query::new(vec![
        Nav::Field("evidence_by_id".into()),
        Nav::Key(Value::Text("seed".into())),
    ]);
    let readers: Vec<_> = (0..4)
        .map(|_| {
            let rt = rt.clone();
            let q = q.clone();
            thread::spawn(move || {
                for _ in 0..80 {
                    assert!(rt.one(&q).unwrap().is_some());
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
