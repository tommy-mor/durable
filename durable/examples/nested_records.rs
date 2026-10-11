//! Nested maps ending in a durable record: `Map<String, Map<String, User>>`.
//!
//! Run with: `cargo run -p durable --example nested_records`
//!
//! The record-layout twin of `nested_maps.rs`. `User` is a `#[derive(Durable)]`
//! struct, so each field gets its own RocksDB key under
//! `orgs/<org>/<username>/<field_id>`. Fields are typed paths: a single field can
//! be read or blind-written without touching the others, but reading a whole
//! user is one point get per field.

use ciborium::Value;
use durable::{query, Db, Describe, Durability, Durable, Leaf, Map, Nav, Query, Write};

#[derive(Durable)]
#[allow(dead_code)]
struct User {
    name: Leaf<String>,
    email: Leaf<String>,
    admin: Leaf<bool>,
}

#[derive(Durable)]
#[allow(dead_code)]
struct Directory {
    /// org -> username -> user record
    orgs: Map<String, Map<String, User>>,
}

/// A record has no single value to `set`; writing one is a write per field.
fn put_user(u: &durable::Path<User>, name: &str, admin: bool) -> Vec<Write> {
    vec![
        u.name().set(&name.to_string()),
        u.email().set(&format!("{}@example.com", name.to_lowercase())),
        u.admin().set(&admin),
    ]
}

fn main() -> durable::Result<()> {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path())?;

    let orgs = Directory::root().orgs();
    let member = |org: &str, user: &str| orgs.key(&org.to_string()).key(&user.to_string());

    let mut writes = Vec::new();
    for (org, user, name, admin) in [
        ("acme", "alice", "Alice", true),
        ("acme", "bob", "Bob", false),
        ("globex", "carol", "Carol", true),
        ("globex", "dave", "Dave", false),
        ("initech", "erin", "Erin", false),
    ] {
        writes.extend(put_user(&member(org, user), name, admin));
    }
    db.apply(&writes, Durability::SyncWal)?;

    // Field-level point read: one key, no decode of the rest of the user.
    println!("acme/alice email = {:?}", member("acme", "alice").email().get(&db)?);

    // Both map levels now hold non-leaf values, so both offer `keys`/`entries`
    // (no `iter`); each user is reassembled from its field paths.
    println!("\norgs: {:?}", orgs.keys(&db)?);
    for (org, members) in orgs.entries(&db)? {
        println!("  {org}:");
        for (username, u) in members.entries(&db)? {
            println!(
                "    {username:<6} {:<20} admin={}",
                u.email().get(&db)?.unwrap_or_default(),
                u.admin().get(&db)?.unwrap_or_default(),
            );
        }
    }

    // Blind single-field write: no read, the other fields are untouched.
    db.run(member("globex", "dave").admin().set(&true), Durability::SyncWal)?;
    println!("\nglobex/dave admin after blind set: {:?}", member("globex", "dave").admin().get(&db)?);

    // Derived records have no `clear`, so deleting a user means deleting each
    // field. A whole org is still one range delete.
    let bob = member("acme", "bob");
    db.apply(
        &[
            bob.name().delete(),
            bob.email().delete(),
            bob.admin().delete(),
            orgs.key(&"initech".to_string()).clear(),
        ],
        Durability::SyncWal,
    )?;
    println!("after removing acme/bob and clearing initech: {:?}", orgs.keys(&db)?);

    // In the query engine, `email` is now a storage step (a PointGet), not a
    // leaf-interior step. `subtree` reassembles the record in one prefix scan.
    let shape = Directory::shape();
    let carol = vec![
        Nav::Field("orgs".into()),
        Nav::Key(Value::Text("globex".into())),
        Nav::Key(Value::Text("carol".into())),
    ];
    let mut email = carol.clone();
    email.push(Nav::Field("email".into()));
    println!("\nquery plan for orgs[globex][carol].email:");
    for step in query::explain(&shape, &Query::new(email.clone()))?.steps {
        println!("  {step}");
    }
    println!("result: {:?}", query::one(&db, &shape, &Query::new(email))?);
    println!("subtree orgs[globex][carol]: {:?}", query::subtree(&db, &shape, &Query::new(carol))?);

    Ok(())
}
