# durable/hop

This repo is two things.
One replaces the backend, one replaces the frontend.
The backend part is the `durable` crate. The frontend part it she `hop` crate.

# durable

A program is a stream of events. Mouse clicks, form submissions, timer ticks.
Then, there are queries into the application. Display all paginated transactions, show me my profile, count how many followers I have.
The simplest possible thing to do, is to drop every event into a .jsonl file as they come in so you don't lose them. Then, when someone queries, scan the tape to answer the query. For many practical problems, this suffices.
But at a certain point, a profile page request should not requiere a full scan looking for the event that initialized it.
This suggests that you maintain in the backend process memory HashMap<UserId, Profile>.
You can scan the tape on boot, and queries are served instantly. This can scale until you have more than ~1GB worth of user profiles. At that point your application state living in memory doesn't fit, and replays at boot become cumbersome.
You have the persistent jsonl event log, and you have the in-memory projection serving queries. The advantage of this is that your event processor/reducer is simple readable code. You parse a struct, insert it into the appropriate indexes without ever touching an sql string or an ORM. Also, your indexes are fine-tuned to your query patterns, and built from just normal data structures. There's no translatign a queue into an sql table, there's no JSONB column. Your code is simply translating events into projections. Your queries arent' selects and joins, you just calmly walk the appropriate hashmaps, slice the right vectors.

Of course this doesn't scale, becuase the projection is the same size or bigger than the eventlog. Eventually it doesn't fit in memory. This is where rocksdb comes in.
Rocksdb is a key value store. It takes a while to compile and requires a c++ toolchain. You give it bytes and a key, and it will save them to disk, while being very clever about the keyspace.
The goal of my `durable` rust crate is to make interacting with rocksdb-backed bytes on disk as ergonomic as interfacing with normal data strcutures.

Your projection schema is a rust struct with a #[derive(Durable)] on it.

```rust
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
```

The structures can be arbitrarily nested. Anything above the leaf is part of the rocksdb key, anything under is is serialized as CBOR in the rocksdb value byte slot. 

The data is set in a transaction, atomically applied in batches.
The data is queried and set via typesafe methods on the struct.

```rust
// mutate
db.apply(&[
  Directory::root().orgs.key(&org.to_string()).key(&name.to_string()).get(db);
], Durability::SyncWal);

// query 
Directory::root().orgs.key(&org.to_string()).key(&name.to_string()).get(db);
```


This api lets you build disk-backed fast structure almost as ergonomically as you would modify an in-memory data structure.

In this way, jsonl and rocksdb can be used as the entire data layer for an application.
There is more to be said about durable, but check out `durable/examples/nested_records.rs` to see more of the api.

# hop

The data layer is only one half of an application. The other half is the user interface. At some point your bits on disk have to be converted to divs in the dom. 

