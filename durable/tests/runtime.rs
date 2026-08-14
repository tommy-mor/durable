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
    let mut rt = open_rt(&dir);

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
    let mut rt = open_rt(&dir);
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
        let mut rt = open_rt(&dir);
        rt.append(Event::Evidence(Evidence {
            id: "e1".into(),
            kind: "llm.judgment".into(),
            epoch: 1,
        }))
        .unwrap();
    }
    // Reopen: projection exists and should catch up (no-op) then accept more.
    let mut rt = open_rt(&dir);
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
    let mut rt = open_rt(&dir);
    for i in 0..50 {
        rt.append(Event::Evidence(Evidence {
            id: format!("e{i}"),
            kind: "llm.judgment".into(),
            epoch: i,
        }))
        .unwrap();
    }
    assert_eq!(rt.log().len().unwrap(), 50);
    assert_eq!(rt.applied().unwrap(), 50);
    rt.verify().unwrap();
}

