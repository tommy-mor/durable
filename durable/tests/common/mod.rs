//! Shared fixtures for the migration-machinery tests: a small projection
//! schema, three event enums with different serde tagging, reducers that fold
//! all three into the same schema, and tape-building helpers.

#![allow(dead_code)]

use std::path::{Path, PathBuf};

use durable::runtime::{render_sealed, seal_crc};
use durable::{Durability, Durable, Leaf, Map, Sum, Tx};
use serde::{Deserialize, Serialize};

/// The projection every fixture folds into.
#[derive(Durable)]
pub struct Totals {
    pub sums: Map<String, Sum<u64>>,
    pub count: Sum<u64>,
    pub last_seq: Leaf<u64>,
}

/// The payload shared by all three enums, so they fold identically.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Pay {
    pub key: String,
    pub n: u64,
}

/// Externally tagged (serde's default): unit, newtype and struct variants.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum Ext {
    Add { key: String, n: u64 },
    Note(String),
    Reset,
    Pair { a: u64, b: String },
}

/// Internally tagged (`type`): struct, unit and newtype-of-struct variants.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Int {
    Add { key: String, n: u64 },
    Note { text: String },
    Reset,
    Wrap(Pay),
}

/// Adjacently tagged (`t` / `c`): newtype and unit variants.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "t", content = "c", rename_all = "snake_case")]
pub enum Adj {
    Add(Pay),
    Note(String),
    Reset,
}

/// What every enum means, once decoded.
pub enum Action {
    Add(String, u64),
    Note,
    Reset,
}

pub fn apply(tx: &mut Tx, action: Action) -> durable::Result<()> {
    let root = Totals::root();
    match action {
        Action::Add(key, n) => tx.write(root.sums().key(&key).add(n)),
        Action::Note => tx.write(root.count().add(1)),
        Action::Reset => tx.write(root.sums().key(&"reset".to_string()).add(1)),
    }
    tx.write(root.last_seq().set(&tx.seq()));
    Ok(())
}

pub fn reduce_ext(tx: &mut Tx, ev: &Ext) -> durable::Result<()> {
    apply(
        tx,
        match ev {
            Ext::Add { key, n } => Action::Add(key.clone(), *n),
            Ext::Note(_) => Action::Note,
            Ext::Reset => Action::Reset,
            Ext::Pair { a, b } => Action::Add(b.clone(), *a),
        },
    )
}

pub fn reduce_int(tx: &mut Tx, ev: &Int) -> durable::Result<()> {
    apply(
        tx,
        match ev {
            Int::Add { key, n } => Action::Add(key.clone(), *n),
            Int::Note { .. } => Action::Note,
            Int::Reset => Action::Reset,
            Int::Wrap(p) => Action::Add(p.key.clone(), p.n),
        },
    )
}

pub fn reduce_adj(tx: &mut Tx, ev: &Adj) -> durable::Result<()> {
    apply(
        tx,
        match ev {
            Adj::Add(p) => Action::Add(p.key.clone(), p.n),
            Adj::Note(_) => Action::Note,
            Adj::Reset => Action::Reset,
        },
    )
}

/// Reducer over raw JSON, for tapes whose event type is not known (real
/// data): counts by top-level `type` and sums `n` where present.
pub fn reduce_value(tx: &mut Tx, ev: &serde_json::Value) -> durable::Result<()> {
    let kind = ev
        .get("type")
        .and_then(|t| t.as_str())
        .unwrap_or("?")
        .to_string();
    let root = Totals::root();
    tx.write(root.sums().key(&kind).add(1));
    tx.write(root.count().add(1));
    tx.write(root.last_seq().set(&tx.seq()));
    Ok(())
}

pub fn schema() -> durable::Shape {
    <Totals as durable::Describe>::shape()
}

/// Build one sealed line whose event is `raw` JSON, copied byte for byte.
/// Same envelope as `render_sealed`, so it is a valid tape line; unlike
/// `render_sealed` it keeps the event text exactly as given (spacing, key
/// order, escapes), which is what byte-identity tests need.
pub fn sealed_raw(seq: u64, ts_ms: u64, from: u64, to: u64, raw: &str) -> String {
    let prefix =
        format!("{{\"seq\":{seq},\"ts_ms\":{ts_ms},\"from\":{from},\"to\":{to},\"event\":{raw}");
    let crc = seal_crc(prefix.as_bytes());
    format!("{prefix},\"crc\":\"{crc}\"}}")
}

/// Canonical sealed line for an event (serde's compact output).
pub fn sealed<E: Serialize>(seq: u64, ts_ms: u64, from: u64, to: u64, event: &E) -> String {
    render_sealed(seq, ts_ms, from, to, event).expect("serialize event")
}

/// Lay out `batches` as a sealed tape: sequential seqs, `from`/`to` per batch,
/// `ts_ms` supplied per record. Returns the lines (no newlines).
pub fn tape_lines<E: Serialize>(batches: &[Vec<(u64, E)>]) -> Vec<String> {
    let mut lines = Vec::new();
    let mut seq = 0u64;
    for batch in batches {
        let from = seq;
        let to = seq + batch.len() as u64 - 1;
        for (ts, ev) in batch {
            lines.push(sealed(seq, *ts, from, to, ev));
            seq += 1;
        }
    }
    lines
}

pub fn write_lines(path: &Path, lines: &[String]) {
    let mut out = String::new();
    for line in lines {
        out.push_str(line);
        out.push('\n');
    }
    std::fs::write(path, out).expect("write tape");
}

pub fn read_bytes(path: &Path) -> Vec<u8> {
    std::fs::read(path).expect("read tape")
}

/// Directory listing names, sorted: used to prove no temp files are left.
pub fn dir_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .expect("read dir")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

pub fn tmp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "durable-mig-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("mkdir");
    dir
}

/// Durability used by test writers that need an immediate commit.
pub fn sync() -> Durability {
    Durability::SyncWal
}
