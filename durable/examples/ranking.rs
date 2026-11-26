//! A pairwise-ranking scope stored with precise, point-addressable updates.
//!
//! Run with: `cargo run -p durable --example ranking`

use durable::{Db, Deque, Durability, Durable, Leaf, Map, Sum};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug)]
struct Vote {
    winner: u32,
    loser: u32,
    weight: f64,
}

#[derive(Durable)]
#[allow(dead_code)]
struct Scope {
    edges: Map<(u32, u32), Sum<f64>>,
    recent: Deque<Leaf<Vote>>,
    votes: Sum<i64>,
}

#[derive(Durable)]
#[allow(dead_code)]
struct Store {
    scopes: Map<String, Scope>,
}

const RECENT_CAP: u64 = 5;

fn record_vote(db: &Db, scope: &str, vote: Vote) -> durable::Result<()> {
    let s = Store::root().scopes().key(scope);

    // One atomic transaction: bump edge, bump count, push vote, and cap recent window!
    db.txn(Durability::SyncWal, |tx| {
        s.edges().add(tx, (vote.winner, vote.loser), vote.weight)?;
        s.votes().add(tx, 1)?;
        s.recent().push_back(tx, &vote)?;
        s.recent().truncate_front(tx, RECENT_CAP)?;
        Ok(())
    })
}

fn main() -> durable::Result<()> {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path())?;

    for i in 0..8 {
        let (winner, loser) = (i % 3, (i + 1) % 3);
        record_vote(
            &db,
            "rust",
            Vote {
                winner,
                loser,
                weight: 1.0 + (i as f64) * 0.1,
            },
        )?;
    }

    let s = Store::root().scopes().key("rust");
    let r = db.read()?;
    println!("total votes: {}", s.votes().get(&r)?);
    println!("recent window (cap {RECENT_CAP}): {}", s.recent().len(&r)?);

    let mut edges = s.edges().iter(&r)?;
    edges.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
    println!("edges by weight:");
    for ((from, to), weight) in edges {
        println!("  {from} -> {to}: {weight:.1}");
    }

    // Decay every edge by 10% in one atomic transaction.
    db.txn(Durability::SyncWal, |tx| {
        s.edges().transform_values(tx, |_e, w| Some(w * 0.9))
    })?;
    println!(
        "edge (0->1) after decay: {:.3}",
        s.edges().get(&db, (0, 1))?
    );

    Ok(())
}
