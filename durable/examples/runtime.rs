//! Event-sourced runtime: typed Rust reducer + rebuildable redb projection + range reads.
//!
//! Run with: `cargo run -p durable --example runtime`

use durable::{Durable, Leaf, List, Map, Runtime, Tx};
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
            root.events().push(tx, e)?;
            root.evidence_by_id().set(tx, &e.id, e)?;
            root.event_ids_by_kind().key(&e.kind).push(tx, &e.id)?;
        }
    }
    Ok(())
}

fn main() -> durable::Result<()> {
    let dir = tempfile::tempdir().unwrap();
    let rt = Runtime::open::<Store>(dir.path().join("proj"), dir.path().join("log.jsonl"), reduce)?;

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

    let r = rt.read()?;
    let root = Store::root();
    println!("evidence e1: {:?}", root.evidence_by_id().get(&r, "e1")?);
    println!("total events: {}", root.events().len(&r)?);
    let kinds: Vec<String> = root
        .events()
        .iter(&r)?
        .into_iter()
        .map(|e| e.kind)
        .collect();
    println!("all kinds: {kinds:?}");

    rt.verify()?;
    println!("incremental == replay from zero");
    Ok(())
}
