//! Nested maps ending in a leaf: `Map<String, Map<String, Leaf<User>>>`.
//!
//! Run with: `cargo run -p durable --example nested_maps`

use durable::{Db, Durability, Durable, Leaf, Map};
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

    db.txn(Durability::SyncWal, |tx| {
        orgs.key("acme").set(tx, "alice", &user("Alice", true))?;
        orgs.key("acme").set(tx, "bob", &user("Bob", false))?;
        orgs.key("globex").set(tx, "carol", &user("Carol", true))?;
        orgs.key("globex").set(tx, "dave", &user("Dave", false))?;
        orgs.key("initech").set(tx, "erin", &user("Erin", false))?;
        Ok(())
    })?;

    let r = db.read()?;
    println!("acme/alice = {:?}", orgs.key("acme").get(&r, "alice")?);
    println!("\norgs: {:?}", orgs.keys(&r)?);
    for (name, members) in orgs.entries(&r)? {
        let users = members.iter(&r)?;
        println!("  {name}: {} member(s)", users.len());
        for (username, u) in users {
            println!("    {username:<6} {:<20} admin={}", u.email, u.admin);
        }
    }

    // Delete one user and clear a whole org in one transaction.
    db.txn(Durability::SyncWal, |tx| {
        orgs.key("acme").remove(tx, "bob")?;
        orgs.remove(tx, "initech")?;
        Ok(())
    })?;
    let r2 = db.read()?;
    println!("\nafter removing acme/bob and clearing initech:");
    println!("  orgs: {:?}", orgs.keys(&r2)?);
    println!("  acme contains bob? {}", orgs.key("acme").contains(&r2, "bob")?);

    // Prefix search on usernames
    let a_users: Vec<String> = orgs
        .key("acme")
        .prefix_keys(&r2, "al")?
        .collect::<Result<_, _>>()?;
    println!("  acme users starting with 'al': {a_users:?}");

    // Bulk rewrite of one org in one transaction.
    db.txn(Durability::SyncWal, |tx| {
        orgs.key("globex").transform_values(tx, |_, mut u| {
            u.admin = true;
            Some(u)
        })
    })?;
    println!(
        "  globex/dave admin after promote: {}",
        orgs.key("globex").get(&db, "dave")?.unwrap().admin
    );

    Ok(())
}
