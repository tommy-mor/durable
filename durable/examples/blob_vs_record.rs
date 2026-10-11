//! Microbenchmark: a user stored as one `Leaf<UserBlob>` vs a `#[derive(Durable)]`
//! record with one key per field.
//!
//! Run with: `cargo run --release -p durable --example blob_vs_record`
//!
//! Writes use `Durability::DisableWal` so the numbers measure encoding and
//! RocksDB work, not fsync latency (a `SyncWal` commit costs the same fsync in
//! both layouts and would dominate). Reads hit the memtable / block cache: this
//! is the hot-path CPU cost, not cold-disk IO.

use std::hint::black_box;
use std::time::Instant;

use ciborium::Value;
use durable::{query, Db, Describe, Durability, Durable, Leaf, Map, Nav, Path, Query, Write};
use serde::{Deserialize, Serialize};

const N: usize = 10_000;
const SCAN_REPS: usize = 20;
const ORG: &str = "acme";

#[derive(Serialize, Deserialize, Clone)]
struct UserBlob {
    name: String,
    email: String,
    admin: bool,
    bio: String,
}

#[derive(Durable)]
#[allow(dead_code)]
struct User {
    name: Leaf<String>,
    email: Leaf<String>,
    admin: Leaf<bool>,
    bio: Leaf<String>,
}

#[derive(Durable)]
#[allow(dead_code)]
struct BlobDir {
    orgs: Map<String, Map<String, Leaf<UserBlob>>>,
}

#[derive(Durable)]
#[allow(dead_code)]
struct RecordDir {
    orgs: Map<String, Map<String, User>>,
}

fn username(i: usize) -> String {
    format!("user{i:05}")
}

fn make_user(i: usize, bio_len: usize) -> UserBlob {
    UserBlob {
        name: format!("User {i}"),
        email: format!("user{i}@example.com"),
        admin: i % 10 == 0,
        bio: "x".repeat(bio_len),
    }
}

/// Visit every user once, in a scattered (non-key) order.
fn order() -> impl Iterator<Item = usize> {
    (0..N).map(|i| (i * 7919) % N)
}

const RUNS: usize = 3;

/// Best of `RUNS` passes, in ns per op. Every pass is idempotent-enough to repeat
/// (inserts become overwrites, field flips toggle back and forth).
fn time(label: &str, ops: usize, mut f: impl FnMut() -> durable::Result<()>) -> durable::Result<f64> {
    let mut ns = f64::INFINITY;
    for _ in 0..RUNS {
        let start = Instant::now();
        f()?;
        ns = ns.min(start.elapsed().as_nanos() as f64 / ops as f64);
    }
    println!("    {label:<42} {:>8.0} ns/op", ns);
    Ok(ns)
}

fn read_record(u: &Path<User>, db: &Db) -> durable::Result<UserBlob> {
    Ok(UserBlob {
        name: u.name().get(db)?.unwrap(),
        email: u.email().get(db)?.unwrap(),
        admin: u.admin().get(db)?.unwrap(),
        bio: u.bio().get(db)?.unwrap(),
    })
}

fn write_record(u: &Path<User>, v: &UserBlob) -> Vec<Write> {
    vec![
        u.name().set(&v.name),
        u.email().set(&v.email),
        u.admin().set(&v.admin),
        u.bio().set(&v.bio),
    ]
}

fn org_query() -> Query {
    Query::new(vec![Nav::Field("orgs".into()), Nav::Key(Value::Text(ORG.into()))])
}

fn bench_blob(bio_len: usize) -> durable::Result<Vec<f64>> {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path())?;
    let members = BlobDir::root().orgs().key(&ORG.to_string());
    let shape = BlobDir::shape();
    let dw = Durability::DisableWal;
    let mut r = Vec::new();

    println!("  blob  (Leaf<UserBlob>)");
    r.push(time("insert user", N, || {
        for i in 0..N {
            db.run(members.key(&username(i)).set(&make_user(i, bio_len)), dw)?;
        }
        Ok(())
    })?);
    r.push(time("read whole user", N, || {
        for i in order() {
            black_box(members.key(&username(i)).get(&db)?);
        }
        Ok(())
    })?);
    r.push(time("read one field (admin)", N, || {
        for i in order() {
            black_box(members.key(&username(i)).get(&db)?.unwrap().admin);
        }
        Ok(())
    })?);
    r.push(time("update one field (get+set)", N, || {
        for i in order() {
            let u = members.key(&username(i));
            let mut v = u.get(&db)?.unwrap();
            v.admin = !v.admin;
            db.run(u.set(&v), dw)?;
        }
        Ok(())
    })?);
    r.push(time("replace whole user", N, || {
        for i in order() {
            db.run(members.key(&username(i)).set(&make_user(i + 1, bio_len)), dw)?;
        }
        Ok(())
    })?);
    r.push(time("scan org: typed iter (per user)", N * SCAN_REPS, || {
        for _ in 0..SCAN_REPS {
            black_box(members.iter(&db)?);
        }
        Ok(())
    })?);
    r.push(time("scan org: query subtree (per user)", N * SCAN_REPS, || {
        for _ in 0..SCAN_REPS {
            black_box(query::subtree(&db, &shape, &org_query())?);
        }
        Ok(())
    })?);
    Ok(r)
}

fn bench_record(bio_len: usize) -> durable::Result<Vec<f64>> {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path())?;
    let members = RecordDir::root().orgs().key(&ORG.to_string());
    let shape = RecordDir::shape();
    let dw = Durability::DisableWal;
    let mut r = Vec::new();

    println!("  record (#[derive(Durable)] User)");
    r.push(time("insert user", N, || {
        for i in 0..N {
            db.apply(&write_record(&members.key(&username(i)), &make_user(i, bio_len)), dw)?;
        }
        Ok(())
    })?);
    r.push(time("read whole user", N, || {
        for i in order() {
            black_box(read_record(&members.key(&username(i)), &db)?);
        }
        Ok(())
    })?);
    r.push(time("read one field (admin)", N, || {
        for i in order() {
            black_box(members.key(&username(i)).admin().get(&db)?);
        }
        Ok(())
    })?);
    r.push(time("update one field (blind set)", N, || {
        for i in order() {
            db.run(members.key(&username(i)).admin().set(&(i % 2 == 0)), dw)?;
        }
        Ok(())
    })?);
    r.push(time("replace whole user", N, || {
        for i in order() {
            db.apply(&write_record(&members.key(&username(i)), &make_user(i + 1, bio_len)), dw)?;
        }
        Ok(())
    })?);
    r.push(time("scan org: typed entries+gets (per user)", N * SCAN_REPS, || {
        for _ in 0..SCAN_REPS {
            for (_, u) in members.entries(&db)? {
                black_box(read_record(&u, &db)?);
            }
        }
        Ok(())
    })?);
    r.push(time("scan org: query subtree (per user)", N * SCAN_REPS, || {
        for _ in 0..SCAN_REPS {
            black_box(query::subtree(&db, &shape, &org_query())?);
        }
        Ok(())
    })?);
    Ok(r)
}

fn main() -> durable::Result<()> {
    let labels = [
        "insert user",
        "read whole user",
        "read one field",
        "update one field",
        "replace whole user",
        "scan org (typed)",
        "scan org (query subtree)",
    ];
    for bio_len in [16, 4096] {
        println!("\n=== {N} users, bio = {bio_len} bytes ===");
        let blob = bench_blob(bio_len)?;
        let record = bench_record(bio_len)?;
        println!("  summary (record / blob; <1 means record is faster)");
        for (label, (b, r)) in labels.iter().zip(blob.iter().zip(&record)) {
            println!("    {label:<26} {:>6.2}x", r / b);
        }
    }
    Ok(())
}
