# durable — design notes

`durable` is a pure-Rust, `redb`-native event-sourced state runtime.

## Architecture

1. **Source of truth**: a single self-sealing JSONL event tape (`src/runtime.rs`,
   specified in `docs/tape.md`). Every line carries `{seq, ts_ms, from, to,
   [id, idh], event, crc}`. A single pure `scan_reader` function decides the
   committed prefix; torn tails past the committed prefix drop by rule.
2. **Derived projection**: a rebuildable `redb` database (`src/store.rs`),
   written with `Durability::None` during normal appends/replays and
   `Durability::Immediate` on schema wipe/stamp.
3. **Typed schema & ordered keys**: `#[derive(Durable)]` (`durable-derive`),
   `Schema` (`src/schema.rs`), `Key` (`src/key.rs`), and `Path<S>` (`src/path.rs`).

## Table & key layout

At the database root (`Store::root()`), `#[derive(Durable)]` maps:
- each collection or nested-record field to its own `redb` table named after
  the field (or `"{ns}/{field}"` under `Store::namespaced(ns)`),
- top-level scalar fields (`Leaf<T>`, `Sum<N>`) to `"__scalars"`,
- runtime bookkeeping (`applied`, `applied_byte_offset`, `stamp_reducer`,
  `stamp_schema`, `ids/<id>`) to `"__durable"`.

Within a table:
- At the table root (`prefix` empty), child keys sit directly at `encode_key(k)`.
  Flat root collections (`Map<K, Leaf<T>>`, `Map<K, Sum<N>>`, `Set<K>`,
  `List<Leaf<T>>`, `Deque<Leaf<T>>`) have 1 physical row per logical entry, so
  `len()` is an O(1) B-tree header read and `clear()` is an O(1) `delete_table`.
- Under any non-empty parent prefix `P`, children live at `P ++ [0x01] ++ seg`.
  Deleting a nested subtree runs in-place via `redb`'s `Table::retain_in`.

### Ordered `Key` encoding (`src/key.rs`, `src/codec.rs`)

Every `Key` is order-preserving, self-delimiting, and strictly prefix-disjoint:
- Fixed-width big-endian integers (`u8..u128`), sign-bit-flipped big-endian
  signed integers (`i8..i128`), IEEE 754 total-order `OrderedF64`.
- Strings & byte slices escape `0x00 → 0x00 0xFF` and terminate with
  `0x00 0x00`. The two-byte terminator prevents `"a"` (`61 00 00`) from being
  a prefix of sibling `"a\0"` (`61 00 ff 00 00`), keeping subtree deletion,
  `contains`, and `watch` overlap exact even with embedded NUL bytes.
- `Reverse<K>` bitwise-inverts the encoded bytes of `K`.
- Tuples `(A, B)`, `(A, B, C)`, `(A, B, C, D)` concatenate their element
  encodings directly, making leading-element prefixes contiguous ranges.

## Transaction & snapshot model

- **`ReadTx`** (`rt.read()` / `db.read()`): wraps `redb::ReadTransaction`.
  All point lookups and range scans share one MVCC snapshot. Range iterators
  use `ReadOnlyTable::range` (`Range<'static, &[u8], &[u8]>`) backed by
  `Arc<TransactionGuard>` — lazy, streaming, and `DoubleEndedIterator`.
- **`Tx`** (`db.write()` / reducer `&mut Tx`): wraps `redb::WriteTransaction`
  directly. Every write is immediately visible to subsequent reads on the same
  `Tx` in arrival order, and `Tx` records touched `WatchRange`s for reactive
  listeners.
- **Append lifecycle**:
  1. Open `WriteTransaction` (`Durability::None`), reduce all events in the
     batch sequentially. Any reducer error aborts the transaction — the tape
     is untouched.
  2. Render sealed lines, check optional `min_free_bytes` via `statvfs`, write
     and `sync_data` the tape.
  3. Write `applied`, `applied_byte_offset`, and optional idempotency record
     into the `WriteTransaction`, commit atomically, and notify `on_commit`.
- **Replay / rebuild**: streams lines with `BufReader`, seeks from
  `applied_byte_offset`, and commits in 4,096-event chunks. `verify()` replays
  into `Db::open_memory()` (`redb::backends::InMemoryBackend`).
