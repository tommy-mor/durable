# durable

A Rust event-sourced application-state runtime: a typed reducer, a rebuildable
RocksDB projection, and a serializable Specter-shaped read language.

Python (or any other client) may **append an event** and **query**. It cannot
mutate the projection. The only legal state transition is an event the Rust
state machine recognizes.

```text
WRITE:  Event → reducer → Tx → projection
READ:   Query path → engine → value(s)
```

The storage layer underneath is still what durable has always been: deeply
nested, precisely updatable RocksDB structures, built around **paths as data**.
The reducer uses those typed paths directly. There is no generic mutation IR
on the client side of the boundary.

```rust
fn reduce(tx: &mut Tx, event: &Event) -> Result<()> {
    let root = Store::root();
    match event {
        Event::Evidence(e) => {
            tx.write(root.events().push_op(e));
            tx.write(root.evidence_by_id().key(&e.id).set(e));
            tx.write(root.event_ids_by_kind().key(&e.kind).push_op(&e.id));
        }
        Event::Emission(em) => {
            tx.write(root.latest_emission().set(em));
        }
    }
    Ok(())
}
```

```python
view.one(ROOT.evidence_by_id[event_id].payload)
view.select(ROOT.events[WHERE(F.epoch > 4)].kind)
view.query({
    "latest": ROOT.latest_emission,
    "kinds": ROOT.events[ALL].kind,
})
```

What crosses the boundary is plain data — a closed algebra, not a callable:

```text
[null, [[0, "events"], [7, [4, [0, "epoch"], [1, 4]]], [0, "kind"]]]
```

`one` / `select` / `subtree` / `entries` / `project` are explicit terminals.
`q.explain()` tells you whether you bought a point get or a prefix scan.

Startup replays `eventlog[offset+1..]`. Rebuild destroys the projection and
reduces from event 0. The important property test is
`incremental execution == replay from zero`.

The rest of this document is the implementation layer the reducer sits on.

---

Deeply nested, precisely updatable RocksDB-backed data structures for Rust,
built around **paths as data**.

Most embedded-storage wrappers make you serialize a whole struct into one blob.
Updating one field means reading, deserializing, mutating, re-serializing, and
rewriting the entire value. `durable` takes the opposite approach: you describe
your data as a *schema* of composable types, and address any location with a
typed **path**. A path lowers to a deterministic RocksDB key with no I/O, so a
mutation touches exactly the keys it names — nothing else.

```rust
use durable::{Db, Durable, Durability, Leaf, Map, Sum};

#[derive(Durable)]
struct Store {
    scores: Map<String, Sum<i64>>,
    title: Leaf<String>,
}

fn main() -> durable::Result<()> {
    let db = Db::open("scores.db")?;
    let root = Store::root();
    let alice = "alice".to_string();

    // Three precise writes, one atomic batch, one WAL flush.
    db.apply(
        &[
            root.scores().key(&alice).add(10), // blind merge — no read
            root.scores().key(&alice).add(5),
            root.title().set(&"leaderboard".to_string()),
        ],
        Durability::SyncWal,
    )?;

    assert_eq!(root.scores().key(&alice).get(&db)?, 15);
    Ok(())
}
```

## The model

### Schema types

A *schema* is a type-level description of a location's shape. Compose them
freely:

| Type | Meaning | Key terminal ops |
|------|---------|------------------|
| `Leaf<T>` | one CBOR-encoded value | `get`, `set`, `delete` |
| `Map<K, V>` | keys `K` → sub-schema `V` | `key`, `keys`, `entries`, `len`, `contains`, `clear` |
| `List<V>` | index-addressed sequence | `at`, `push`, `pop`, `iter`, `len`, `clear` |
| `Deque<V>` | double-ended queue (O(1) ends) | `push_back`, `push_front`, `pop_front`, `pop_back`, `front`, `back`, `iter` |
| `Sum<N>` | numeric accumulator | `add` (blind merge), `get`, `set` |
| `#[derive(Durable)] struct` | fixed named fields | one navigator method per field |

Leaf- and `Sum`-valued maps additionally get `get`, `iter`, and
`transform_values` (a one-scan bulk rewrite that yields reified writes — e.g.
"decay every edge weight").

Nest them arbitrarily:

```rust
use durable::{Deque, Durable, Leaf, Map, Sum};
# use serde::{Serialize, Deserialize};
# #[derive(Serialize, Deserialize)] struct Vote;
#[derive(Durable)]
#[allow(dead_code)]
struct GroupState {
    edges: Map<(u32, u32), Sum<f64>>,
    recent_votes: Deque<Leaf<Vote>>,
    item_count: Sum<i64>,
}

#[derive(Durable)]
#[allow(dead_code)]
struct Store {
    scopes: Map<String, GroupState>,
}
```

Now `Store::root().scopes().key(&scope).edges().key(&(i, j)).add(1.0)` updates a
single edge weight without reading or rewriting anything else in the scope.

### Paths are data

`Path<S>` is just a byte prefix plus a phantom schema type. Navigation is pure
and allocation-light; nothing hits the database until you read or apply. Because
paths are values you can build them once and reuse them, pass them around, and
compose them.

### Mutations are reified

Terminal mutating operations don't perform side effects — they return a
[`Write`], a typed wrapper around a plain-data [`Op`] (`Put` / `Delete` /
`DeletePrefix` / `Merge`). Collect several and apply them atomically:

```rust,ignore
let writes = vec![
    edges.key(&(0, 1)).add(2.0),
    edges.key(&(1, 0)).add(1.0),
    voted_pairs.key(&(0, 1)).set(&true),
];
db.apply(&writes, Durability::DisableWal)?;
```

Reified writes are inspectable and testable — you can assert on the `Op` a path
produces, log it, or serialize it.

### Blind vs. read-modify-write

The cost model is explicit, not hidden:

- **Blind** (no read): `Leaf::set`/`delete`, `Sum::add`/`set`, `Map::clear`.
  These are pure `Op` data and compose freely in a batch.
- **Read-modify-write**: `List::push`/`pop`, `Deque` pushes/pops (they read a
  length/cursor). In a batch, appends are deferred and resolved at commit so
  several land at contiguous indices in one atomic write.
- **Scan**: `Map::keys`/`iter`/`len`, `transform_values`. Prefix range scans.

`Sum` deserves a special mention: it's backed by a RocksDB associative merge
operator, so `add` is a blind O(1) write whose folding happens lazily during
compaction — ideal for counters and edge weights.

## Durability

Every batch commits with an explicit policy:

- `Durability::SyncWal` — write the WAL and fsync before returning (survives
  power loss).
- `Durability::WalOnly` — write the WAL without forcing an fsync.
- `Durability::DisableWal` — skip the WAL. Use only for projections rebuildable
  from another durable source of truth.

## Key layout

Every location lowers to a key built from length-prefixed segments
(`uvarint(len) ++ bytes`), which makes segment sequences self-delimiting: a
parent prefix only ever prefixes its own descendants, so sibling subtrees never
collide. Within a location prefix `P`:

- `P` (exact) holds a `Leaf`/`Sum` value;
- `P ++ [0x01] ++ seg` holds child data (map entries, struct fields, elements);
- `P ++ [0x00] ++ seg` holds collection metadata (lengths, deque cursors).

Deleting a subtree is a single RocksDB range delete over `[P, upper_bound(P))`.

## What this is not

- Not multi-process safe. One writer process; serialize writes at the app layer.
- Not distributed, not SQL.
- Map iteration order is encoded-byte order, not logical key order.
- On-disk struct field ids come from declaration order — add new fields at the
  end; reordering changes the layout.
- Schema evolution is your responsibility. Because durable shines as a
  *rebuildable projection*, the simplest migration is often to drop the data and
  replay from your canonical log.

## Testing

```bash
cargo test -p durable
```

Covers the codec, the merge operator, every collection kind end-to-end, atomic
batches, durability modes, persistence across reopen, property tests against
`BTreeMap`/`VecDeque`/sum-of-deltas models, the query engine (point / scan /
filter / project / explain), and the runtime property
`incremental execution == replay from zero`.

```bash
cargo run -p durable --example runtime
```

## Hop apps

[`hoprt/`](../hoprt/) is the Lua/browser runtime: placement marks hop a
function body between a browser tab and the server. When a `.hop` file
declares `schema` and `fn reduce`, the server VM opens this crate's
`Runtime` — JSONL log, typed-path projection, `store.append` from server
segments only. The multiplayer todo app is the full loop:

```bash
cargo run -p hoprt --bin hopd -- hoprt/hop/todo.hop
# open http://localhost:9000 — restart hopd; the board is still there
```
