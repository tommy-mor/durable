//! A pairwise-ranking scope stored with precise, point-addressable updates.
//!
//! Run with: `cargo run -p durable --example ranking`
//!
//! This mirrors the motivating use case: a "scope" holds an edge-weight graph, a
//! capped window of recent votes, and a counter. A vote updates a handful of
//! keys in one atomic batch — it never reads or rewrites the whole scope.

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
    /// Directed edge weights: (from, to) -> accumulated weight.
    edges: Map<(u32, u32), Sum<f64>>,
    /// Most recent votes, newest at the back.
    recent: Deque<Leaf<Vote>>,
    /// Total votes recorded in this scope.
    votes: Sum<i64>,
}

#[derive(Durable)]
#[allow(dead_code)]
struct Store {
    scopes: Map<String, Scope>,
}

const RECENT_CAP: u64 = 5;

fn record_vote(db: &Db, scope: &str, vote: Vote) -> durable::Result<()> {
    let s = Store::root().scopes().key(&scope.to_string());

    // One atomic batch: bump the winning edge, flag the count, push the vote.
    let mut batch = db.batch();
    batch.write(s.edges().key(&(vote.winner, vote.loser)).add(vote.weight));
    batch.write(s.votes().add(1));
    batch.push_back(&s.recent(), &vote)?;
    batch.commit_with(Durability::SyncWal)?;

    // Keep only the most recent N votes (O(1) per eviction).
    while s.recent().len(db)? > RECENT_CAP {
        s.recent().pop_front(db)?;
    }
    Ok(())
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

    let s = Store::root().scopes().key(&"rust".to_string());

    println!("total votes: {}", s.votes().get(&db)?);
    println!("recent window (cap {RECENT_CAP}): {}", s.recent().len(&db)?);

    let mut edges = s.edges().iter(&db)?;
    edges.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
    println!("edges by weight:");
    for ((from, to), weight) in edges {
        println!("  {from} -> {to}: {weight:.1}");
    }

    // Decay every edge by 10% in a single scan + atomic batch.
    let decay = s
        .edges()
        .transform_values(&db, |_e, w| Some(w * 0.9))?;
    db.apply(&decay, Durability::SyncWal)?;
    println!(
        "edge (0->1) after decay: {:.3}",
        s.edges().key(&(0, 1)).get(&db)?
    );

    Ok(())
}
