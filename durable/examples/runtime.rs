//! Event-sourced runtime: Python-shaped reads, Rust-only writes.
//!
//! Run with: `cargo run -p durable --example runtime`

use durable::{
    Durable, Leaf, List, Map, Nav, Query, Runtime, Tx,
};
use serde::{Deserialize, Serialize};

#[derive(Durable)]
#[allow(dead_code)]
struct Store {
    events: List<Leaf<Evidence>>,
    evidence_by_id: Map<String, Leaf<Evidence>>,
    event_ids_by_kind: Map<String, List<Leaf<String>>>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "type", rename_all = "lowercase")]
enum Event {
    Evidence(Evidence),
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct Evidence {
    id: String,
    kind: String,
    epoch: u64,
}

fn reduce(tx: &mut Tx, event: &Event) -> durable::Result<()> {
    let root = Store::root();
    match event {
        Event::Evidence(e) => {
            tx.write(root.events().push_op(e));
            tx.write(root.evidence_by_id().key(&e.id).set(e));
            tx.write(root.event_ids_by_kind().key(&e.kind).push_op(&e.id));
        }
    }
    Ok(())
}

fn main() -> durable::Result<()> {
    let dir = tempfile::tempdir().unwrap();
    let mut rt = Runtime::open_described::<Store>(
        dir.path().join("proj"),
        dir.path().join("log.jsonl"),
        None,
        reduce,
    )?;

    rt.append(Event::Evidence(Evidence {
        id: "e1".into(),
        kind: "llm.judgment".into(),
        epoch: 4,
    }))?;
    rt.append(Event::Evidence(Evidence {
        id: "e2".into(),
        kind: "git.commit".into(),
        epoch: 12,
    }))?;

    let kind = rt.one(&Query::new(vec![
        Nav::Field("evidence_by_id".into()),
        Nav::Key(ciborium::Value::Text("e1".into())),
        Nav::Field("kind".into()),
    ]))?;
    println!("evidence e1 kind: {kind:?}");

    let kinds = rt.select(&Query::new(vec![
        Nav::Field("events".into()),
        Nav::All,
        Nav::Field("kind".into()),
    ]))?;
    println!("all kinds: {kinds:?}");

    println!("{}", rt.explain(&Query::new(vec![Nav::Field("events".into()), Nav::All]))?);
    rt.verify()?;
    println!("incremental == replay from zero");
    Ok(())
}
