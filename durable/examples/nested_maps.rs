//! Nested maps ending in a leaf: `Map<String, Map<String, Leaf<User>>>`.
//!
//! Run with: `cargo run -p durable --example nested_maps`
//!
//! The outer map is keyed by org, the inner map by username, and each `User`
//! is stored as one CBOR blob. Both map levels are part of the RocksDB key;
//! the `User`'s own fields are not, so they can only be read or replaced whole
//! (or reached by the query engine as "leaf interior" steps).

use ciborium::Value;
use durable::{query, Db, Describe, Durability, Durable, Leaf, Map, Nav, Query};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
struct User {
    name: String,
    email: String,
    admin: bool,
}

#[derive(Durable)]
#[allow(dead_code)]
struct Directory {
    /// org -> username -> user
    orgs: Map<String, Map<String, Leaf<User>>>,
}

fn user(name: &str, admin: bool) -> User {
    User {
        name: name.to_string(),
        email: format!("{}@example.com", name.to_lowercase()),
        admin,
    }
}

fn main() -> durable::Result<()> {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path())?;

    let orgs = Directory::root().orgs();
    let org = |name: &str| orgs.key(&name.to_string());
    let s = |x: &str| x.to_string();

    // Each set addresses exactly one key: orgs/<org>/<username>.
    db.apply(
        &[
            org("acme").key(&s("alice")).set(&user("Alice", true)),
            org("acme").key(&s("bob")).set(&user("Bob", false)),
            org("globex").key(&s("carol")).set(&user("Carol", true)),
            org("globex").key(&s("dave")).set(&user("Dave", false)),
            org("initech").key(&s("erin")).set(&user("Erin", false)),
        ],
        Durability::SyncWal,
    )?;

    // Point read, through both maps.
    let alice = org("acme").key(&s("alice")).get(&db)?;
    println!("acme/alice = {alice:?}");

    // The outer map's values are maps, so it offers `keys`/`entries`, not `iter`.
    // `entries` yields a typed sub-path per org.
    println!("\norgs: {:?}", orgs.keys(&db)?);
    for (name, members) in orgs.entries(&db)? {
        // The inner map is leaf-valued, so it has `iter` / `get` / `transform_values`.
        let users = members.iter(&db)?;
        println!("  {name}: {} member(s)", users.len());
        for (username, u) in users {
            println!("    {username:<6} {:<20} admin={}", u.email, u.admin);
        }
    }

    // Blind delete of one user, and a range delete of a whole org.
    db.apply(
        &[org("acme").key(&s("bob")).delete(), org("initech").clear()],
        Durability::SyncWal,
    )?;
    println!("\nafter removing acme/bob and clearing initech:");
    println!("  orgs: {:?}", orgs.keys(&db)?);
    println!("  acme contains bob? {}", org("acme").contains(&db, &s("bob"))?);

    // Bulk rewrite of one org: one scan, one atomic batch of blind writes.
    let promote = org("globex").transform_values(&db, |_, mut u| {
        u.admin = true;
        Some(u)
    })?;
    db.apply(&promote, Durability::SyncWal)?;
    println!(
        "  globex/dave admin after promote: {}",
        org("globex").get(&db, &s("dave"))?.unwrap().admin
    );

    // The same data through the query engine. `orgs` and both keys are
    // storage navigation; `email` is a step inside the decoded User blob.
    let shape = Directory::shape();
    let q = Query::new(vec![
        Nav::Field(s("orgs")),
        Nav::Key(Value::Text(s("globex"))),
        Nav::Key(Value::Text(s("carol"))),
        Nav::Field(s("email")),
    ]);
    println!("\nquery plan for orgs[globex][carol].email:");
    for step in query::explain(&shape, &q)?.steps {
        println!("  {step}");
    }
    println!("result: {:?}", query::one(&db, &shape, &q)?);

    Ok(())
}
