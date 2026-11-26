//! Microbenchmark: a user stored as one `Leaf<UserBlob>` vs a `#[derive(Durable)]`
//! record with one key per field, running on `redb`.
//!
//! Run with: `cargo run --release -p durable --example blob_vs_record`

use std::hint::black_box;
use std::time::Instant;

use durable::{Db, Durability, Durable, Leaf, Map, Path, Read, Tx};
use serde::{Deserialize, Serialize};

const N: usize = 5_000;
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

fn read_record(u: &Path<User>, r: &impl Read) -> durable::Result<UserBlob> {
    Ok(UserBlob {
        name: u.name().get(r)?.unwrap(),
        email: u.email().get(r)?.unwrap(),
        admin: u.admin().get(r)?.unwrap(),
        bio: u.bio().get(r)?.unwrap(),
    })
}

fn write_record(tx: &mut Tx, u: &Path<User>, v: &UserBlob) -> durable::Result<()> {
    u.name().set(tx, &v.name)?;
    u.email().set(tx, &v.email)?;
    u.admin().set(tx, &v.admin)?;
    u.bio().set(tx, &v.bio)?;
    Ok(())
}

fn time(label: &str, ops: usize, mut f: impl FnMut() -> durable::Result<()>) -> durable::Result<()> {
    let start = Instant::now();
    f()?;
    let ns = start.elapsed().as_nanos() as f64 / ops as f64;
    println!("  {label:<40} {:>8.0} ns/op", ns);
    Ok(())
}

fn main() -> durable::Result<()> {
    for bio_len in [64, 4096] {
        println!("\n=== N={N}, bio_len={bio_len} ===");

        let db_blob = Db::open_memory()?;
        let members = BlobDir::root().orgs().key(ORG);
        time("blob: bulk insert", N, || {
            db_blob.txn(Durability::DisableWal, |tx| {
                for i in 0..N {
                    members.set(tx, username(i), &make_user(i, bio_len))?;
                }
                Ok(())
            })
        })?;
        time("blob: read 1 field (email)", N, || {
            let r = db_blob.read()?;
            for i in 0..N {
                black_box(members.get(&r, username(i))?.unwrap().email);
            }
            Ok(())
        })?;
        time("blob: flip 1 field (admin)", N, || {
            db_blob.txn(Durability::DisableWal, |tx| {
                for i in 0..N {
                    let k = username(i);
                    let mut u = members.get(tx, &k)?.unwrap();
                    u.admin = !u.admin;
                    members.set(tx, &k, &u)?;
                }
                Ok(())
            })
        })?;

        let db_rec = Db::open_memory()?;
        let rmembers = RecordDir::root().orgs().key(ORG);
        time("record: bulk insert", N, || {
            db_rec.txn(Durability::DisableWal, |tx| {
                for i in 0..N {
                    write_record(tx, &rmembers.key(username(i)), &make_user(i, bio_len))?;
                }
                Ok(())
            })
        })?;
        time("record: read 1 field (email)", N, || {
            let r = db_rec.read()?;
            for i in 0..N {
                black_box(rmembers.key(username(i)).email().get(&r)?.unwrap());
            }
            Ok(())
        })?;
        time("record: read whole user", N, || {
            let r = db_rec.read()?;
            for i in 0..N {
                black_box(read_record(&rmembers.key(username(i)), &r)?);
            }
            Ok(())
        })?;
        time("record: flip 1 field (admin)", N, || {
            db_rec.txn(Durability::DisableWal, |tx| {
                for i in 0..N {
                    let u = rmembers.key(username(i));
                    let a = u.admin().get(tx)?.unwrap();
                    u.admin().set(tx, &!a)?;
                }
                Ok(())
            })
        })?;
    }
    Ok(())
}
