# durable

A pure-Rust event-sourced application-state runtime: a self-sealing JSONL event
tape, a typed reducer, and a rebuildable `redb` projection built around **typed
paths** and **ordered keys**.

```text
WRITE:  Event(s) → reduce into redb WriteTx (validated, read-your-writes)
                 → fsync sealed JSONL tape
                 → commit WriteTx atomically (+ applied offset + idempotency key)
READ:   rt.read()? → MVCC ReadTx snapshot → Path::get / range / prefix / page / len
```

## Example

```rust
use durable::{Dir, Durable, Leaf, Map, Runtime, Set, Sum, Tx};
use serde::{Deserialize, Serialize};

#[derive(Durable)]
struct Store {
    users: Map<String, Leaf<String>>,
    user_votes: Map<String, Set<i64>>,
    identities: Map<(String, String), Leaf<String>>,
    total_votes: Sum<u64>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
enum Event {
    AddUser { name: String, bio: String },
    CastVote { id: i64, user: String },
}

fn reduce(tx: &mut Tx, event: &Event) -> durable::Result<()> {
    let root = Store::root();
    match event {
        Event::AddUser { name, bio } => root.users().set(tx, name, bio)?,
        Event::CastVote { id, user } => {
            root.user_votes().key(user).insert(tx, *id)?;
            root.total_votes().add(tx, 1)?;
        }
    }
    Ok(())
}
```

Read from a consistent MVCC snapshot (`rt.read()?`):

```rust
let r = rt.read()?;
let root = Store::root();

// O(1) count on top-level flat tables (no scan, no decode)
let n_users = root.users().len(&r)?;

// Lazy, streaming, double-ended range reads & cursor pagination
let (newest_50, next_cursor) =
    root.user_votes().key("tommy").page(&r, None::<i64>, 50, Dir::Desc)?;

// Lexicographic prefix search on string keys
let autocomplete: Vec<String> =
    root.users().prefix_keys(&r, "tom")?.take(10).collect::<Result<_, _>>()?;

// Composite tuple prefix scan
let github_users: Vec<_> =
    root.identities().prefix(&r, ("github",))?.collect::<Result<_, _>>()?;
```

## Schema types

Compose freely to arbitrary depth:

| Type | Meaning | Key operations |
|------|---------|----------------|
| `Leaf<T>` | CBOR-encoded scalar | `get`, `set`, `update`, `delete` |
| `Sum<N>` | numeric accumulator (`i64`, `u64`, `f64`) | `get`, `add`, `set`, `delete` |
| `Map<K, V>` | ordered map `K: Key` → sub-schema `V` | `key`, `get`, `get_many`, `set`, `remove`, `contains`, `len`/`count`, `count_range`, `first`, `last`, `range`, `prefix`, `page`, `keys`, `iter`, `entries`, `transform_values`, `clear` |
| `Set<K>` | ordered set of `K: Key` (0-byte values) | `insert`, `remove`, `contains`, `len`/`count`, `count_range`, `first`, `last`, `range`, `prefix`, `page`, `iter`, `clear` |
| `List<V>` | index-addressed sequence (`u64`) | `at`, `get`, `first`, `last`, `push`, `pop`, `range`, `iter`, `len`, `clear` |
| `Deque<V>` | double-ended queue (`i64`, O(1) ends) | `push_back`, `push_front`, `pop_front`, `pop_back`, `front`, `back`, `truncate_front`, `truncate_back`, `range`, `iter`, `len`, `clear` |
| `#[derive(Durable)] struct` | fixed named fields | one navigator method per field, plus `clear` / `exists` / `watch_range` |

## Key ordering and layout

Every map/set key implements `durable::Key`, which encodes values in **natural
`Ord` byte order** while remaining self-delimiting and prefix-disjoint:

- **Unsigned integers (`u8`..`u128`)**: big-endian.
- **Signed integers (`i8`..`i128`)**: sign-bit-flipped big-endian (`i64::MIN < -1 < 0 < 1 < i64::MAX`).
- **Floats (`OrderedF64`)**: IEEE 754 `total_cmp` order (suitable for scores/leaderboards).
- **Strings (`String` / `&str`) and bytes (`Vec<u8>` / `&[u8]`)**: exact lexicographic UTF-8/byte order (`"aaa" < "zz"`), escaping `0x00 → 0x00 0xFF` and terminating with `0x00 0x00` so `"a"` is never a byte-prefix of `"a\0"`.
- **Descending order (`Reverse<K>`)**: bitwise-inverted bytes (`Reverse(100) < Reverse(50)`).
- **Composite tuples (`(A, B)`, `(A, B, C)`, `(A, B, C, D)`)**: concatenated ordered elements, supporting leading-element prefix scans (`prefix(&("github",))`).

At the root of a `#[derive(Durable)]` store, each collection field gets its own
`redb` table named after the field (scalars share `"__scalars"`, bookkeeping
lives in `"__durable"`):

- Root flat collections (`Map<K, Leaf<T>>`, `Map<K, Sum<N>>`, `Set<K>`, `List<Leaf<T>>`, `Deque<Leaf<T>>`) get **O(1) `len()`** (B-tree header count) and **O(1) `clear()`** (`delete_table`).
- Nested subtrees live under their parent's table prefix with `[0x01]` (`DATA`) separation and clear in-place via `retain_in`.
- `Db::open_memory()` runs the exact same `redb` engine over `redb::backends::InMemoryBackend` for fast, zero-disk tests and offline verification.

## Runtime guarantees

- **Reduce-first validation**: `append` and `append_batch` run the reducer inside an uncommitted `redb::WriteTransaction` with full read-your-writes *before* touching the tape. A rejected event aborts the transaction cleanly — poison events never hit the tape.
- **Atomic batch visibility**: all events in `append_batch` commit in one `redb::WriteTransaction`. Concurrent `ReadTx` snapshots never see half a batch.
- **Idempotency**: `append_idempotent(id, event)` / `append_batch_idempotent(id, events)` deduplicate retries (`AppendOutcome::Duplicate`) and reject payload mismatches (`Error::Conflict`), surviving restarts and rebuilds.
- **Halted on replay error**: a reducer error during replay of an already-committed tape event halts the runtime (`Error::Halted { seq, reason }`) and refuses further appends until reopened with a fixed reducer.
- **Streaming tape I/O & fast catch-up**: scanning, replay, and rebuild stream line-by-line (O(1) RAM), seek from `applied_byte_offset`, and commit in 4,096-event chunks.
- **Automatic schema rebuild**: `Runtime::open` / `Runtime::open_with` stamps `{ProjectionId, Schema::fingerprint()}` (covering format version, field layout, key types, sum types, and leaf type names) and rebuilds automatically on mismatch (or refuses under `OnMismatch::Refuse`).
- **Typed reactive watch**: `path.watch_range()` + `rt.on_commit(|seq, touched| watch_hit(&w, touched))`.
- **Live backups**: `rt.backup_tape(dst)` atomically copies the exact committed prefix `[0..committed_end]`.

## Evolving a tape

Bump `ProjectionId` to re-derive the projection, record fold decisions as data,
and rewrite old records offline with `durable::rewrite` (proven equal by
`durable::testing::assert_rewrite_equivalent`). Read `docs/evolving-a-tape.md`.
Operator CLI: `durable-tape status|verify|cat|census` and `durable-tape-migrate`.

## Testing

```bash
cargo test --workspace
```
