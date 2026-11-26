# durable

A program is a stream of events. Mouse clicks, form submissions, timer ticks.
Then, there are queries into the application. Display all paginated transactions, show me my profile, count how many followers I have.

The simplest possible thing to do is to drop every event into a `.jsonl` file as they come in so you don't lose them. Then, when someone queries, scan the tape to answer the query. For many practical problems, this suffices.

At a certain point, a profile page request should not require a full scan looking for the event that initialized it. That suggests maintaining an in-memory `HashMap<UserId, Profile>` rebuilt on boot. Your reducer is simple, readable code — you parse an event and insert into normal data structures without touching SQL or an ORM.

Eventually the projection outgrows memory (or boot replays take too long). `durable` pairs a **self-sealing JSONL event tape** with a **rebuildable `redb` projection** exposed as typed, ordered Rust data structures.

Your projection schema is a Rust struct with `#[derive(Durable)]`:

```rust
use durable::{Db, Dir, Durability, Durable, Leaf, Map, Set, Sum};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
struct User {
    name: String,
    email: String,
    admin: bool,
}

#[derive(Durable)]
struct Directory {
    /// org -> username -> user (lexicographically ordered)
    orgs: Map<String, Map<String, Leaf<User>>>,
    /// global ordered set of tags (O(1) len & clear)
    tags: Set<String>,
    /// total user count
    user_count: Sum<u64>,
}
```

Each top-level collection maps to its own `redb` table (giving O(1) `len()` and `clear()`), and keys use an order-preserving binary encoding (`String`, signed/unsigned ints, `OrderedF64`, `Reverse<K>`, and composite tuples `(A, B)`).

Mutations run atomically in a read-your-writes `Tx`; reads run against a consistent MVCC `ReadTx` snapshot with streaming double-ended range scans, prefix search, and cursor pagination:

```rust
// mutate atomically (or inside a Runtime reducer)
db.txn(Durability::SyncWal, |tx| {
    Directory::root().orgs().key("acme").set(tx, "alice", &alice)?;
    Directory::root().tags().insert(tx, "rust")?;
    Directory::root().user_count().add(tx, 1)?;
    Ok(())
})?;

// snapshot-consistent reads, range scans, and prefix autocomplete
let r = db.read()?;
let alice = Directory::root().orgs().key("acme").get(&r, "alice")?;
let (page, cursor) = Directory::root().orgs().key("acme").page(&r, None::<&str>, 50, Dir::Desc)?;
let matches: Vec<String> = Directory::root().orgs().key("acme").prefix_keys(&r, "al")?.collect::<Result<_, _>>()?;
```

In this way, a self-sealing `.jsonl` tape and `redb` serve as the entire data layer for a Rust application. See [`durable/README.md`](durable/README.md) and `durable/examples/` for more.
