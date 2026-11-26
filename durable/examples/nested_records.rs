//! Nested maps ending in a durable record: `Map<String, Map<String, User>>`.
//!
//! Run with: `cargo run -p durable --example nested_records`

use durable::{Db, Durability, Durable, Leaf, Map, Path, Tx};

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
    orgs: Map<String, Map<String, User>>,
}

fn put_user(tx: &mut Tx, u: &Path<User>, name: &str, admin: bool) -> durable::Result<()> {
    u.name().set(tx, &name.to_string())?;
    u.email()
        .set(tx, &format!("{}@example.com", name.to_lowercase()))?;
    u.admin().set(tx, &admin)?;
    Ok(())
}

fn main() -> durable::Result<()> {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path())?;
    let orgs = Directory::root().orgs();

    db.txn(Durability::SyncWal, |tx| {
        for (org, user, name, admin) in [
            ("acme", "alice", "Alice", true),
            ("acme", "bob", "Bob", false),
            ("globex", "carol", "Carol", true),
            ("globex", "dave", "Dave", false),
            ("initech", "erin", "Erin", false),
        ] {
            put_user(tx, &orgs.key(org).key(user), name, admin)?;
        }
        Ok(())
    })?;

    let r = db.read()?;
    println!(
        "acme/alice email = {:?}",
        orgs.key("acme").key("alice").email().get(&r)?
    );

    println!("\norgs: {:?}", orgs.keys(&r)?);
    for (org, members) in orgs.entries(&r)? {
        println!("  {org}:");
        for (username, u) in members.entries(&r)? {
            println!(
                "    {username:<6} {:<20} admin={}",
                u.email().get(&r)?.unwrap_or_default(),
                u.admin().get(&r)?.unwrap_or_default(),
            );
        }
    }

    // Single-field write, typed record deletion (`remove`), and org clear in one txn.
    db.txn(Durability::SyncWal, |tx| {
        orgs.key("globex").key("dave").admin().set(tx, &true)?;
        orgs.key("acme").remove(tx, "bob")?;
        orgs.remove(tx, "initech")?;
        Ok(())
    })?;
    let r2 = db.read()?;
    println!(
        "\nglobex/dave admin: {:?}",
        orgs.key("globex").key("dave").admin().get(&r2)?
    );
    println!(
        "after removing acme/bob and initech, orgs: {:?}",
        orgs.keys(&r2)?
    );
    println!(
        "acme contains bob? {}",
        orgs.key("acme").contains(&r2, "bob")?
    );

    Ok(())
}
