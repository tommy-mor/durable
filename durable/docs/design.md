# durable — design notes

This document describes how `durable` actually works, so the layout and cost
model are auditable rather than mysterious.

## Goals

1. **Precise updates.** A mutation touches only the keys it names. No
   read-deserialize-mutate-reserialize-write of a whole struct.
2. **Deep nesting.** Maps, lists, deques, and structs compose to arbitrary
   depth, all in one RocksDB column family.
3. **Type safety.** Illegal navigation and illegal operations fail to compile.
4. **Paths and mutations as data.** Addresses and edits are values you can
   build, reuse, inspect, and apply in atomic batches.

Non-goals: multi-process concurrency, distribution, SQL, ad-hoc range queries
over logical key order.

## Key encoding

A location is a sequence of **segments**. Each segment is length-prefixed:
`uvarint(len) ++ bytes`. The full RocksDB key is the concatenation of a parent
prefix and a one-byte discriminator plus a segment per step.

Length-prefixing makes segment sequences *self-delimiting*: no segment can be a
byte-prefix of a different segment, so a parent prefix only ever prefixes its own
descendants. Sibling subtrees never overlap.

Within a location prefix `P`:

| Key | Holds |
|-----|-------|
| `P` (exact) | a `Leaf` / `Sum` scalar value |
| `P ++ [0x01] ++ seg` | child data: map entry, struct field, list/deque element |
| `P ++ [0x00] ++ seg` | collection metadata: list `len`, deque `head`/`tail` |

- **Map** entry under key `k`: segment is `cbor(k)`. Iteration is a range scan
  over `P ++ [0x01]`; logical keys are deduplicated by their first segment
  (nested values contribute several physical keys sharing that segment).
- **List** element `i`: segment is `i` as 8 big-endian bytes; `len` lives in
  metadata.
- **Deque** element `i` (an `i64`, possibly negative): segment is an
  order-preserving encoding (`(i as u64) ^ (1<<63)` big-endian) so the byte order
  matches signed numeric order. `head`/`tail` cursors live in metadata; both ends
  are O(1) and never renumber.
- **Struct** field: segment is the field's declaration-order id as a uvarint.

Deleting a subtree is one RocksDB range delete over `[P, prefix_upper_bound(P))`
(falling back to a scan only when the prefix is empty or all `0xff`).

## Types and navigation

`Path<S>` carries the lowered prefix bytes and a phantom schema `S`. Navigation
methods are implemented per concrete schema, so `Path<Map<K, V>>` has `key`,
`Path<List<V>>` has `at`, a derived struct's `Path` has its field navigators, and
so on. Each step appends a segment and returns a `Path` of the child schema.

`#[derive(Durable)]` generates, for a struct, the `Schema` impl, a `{Name}Fields`
extension trait of navigators implemented for `Path<Name>`, and `Name::root()` /
`Name::namespaced(name)` constructors.

## Mutations and the cost model

Terminal mutating operations return reified `Write`s wrapping a plain-data `Op`
(`Put` / `Delete` / `DeletePrefix` / `Merge`). They are applied via `Db::apply`
or pushed onto a `Batch`, which commits as a single RocksDB write with an
explicit `Durability`.

- **Blind** ops carry fully-determined keys and never read: `Leaf::set`/`delete`,
  `Sum::add`/`set`/`delete`, collection `clear`.
- **Read-modify-write** ops read a length or cursor: list/deque pushes and pops.
  In a `Batch`, appends are deferred and resolved at commit so contiguous appends
  get contiguous indices and the whole batch is one atomic write.
- **Scans**: `keys`/`iter`/`len`/`contains`/`transform_values`.

### Sum and the merge operator

`Sum<N>` is backed by a RocksDB associative merge operator registered at
`Db::open`. Accumulator values are stored tagged (`[type_tag, 8 LE bytes]`) so a
single operator folds `f64`, `i64`, and `u64` correctly. `add(delta)` is a blind
`Merge` write: O(1), no read, folded lazily during compaction. This is the right
primitive for counters and graph edge weights.

## Durability and recovery

`SyncWal` fsyncs the WAL before returning; `WalOnly` writes the WAL without an
fsync; `DisableWal` skips it. `DisableWal` is intended for projections that can
be rebuilt from another durable source of truth — its writes may be lost on an
unclean crash.

`durable` is deliberately not a recovery plan on its own. Pair it with a
canonical log if you need crash recovery, and prefer "drop and replay" over
in-place migration when a schema changes.

## Runtime

[`Runtime<E>`](../src/runtime.rs) is that pairing. The JSONL log is the source
of truth. Callers supply an event body; the runtime stamps a [`Record`] with a
sequence number and a monotonic millisecond timestamp, then fsyncs that line
(or a whole [`append_batch`] group) before reducing. The reducer receives a
[`Tx`] whose [`Tx::meta`] is that stamp — ids and times are not minted by the
client. The applied offset is committed in the same RocksDB batch as the
event's projection writes, so a crash before commit replays the event against
the previous projection. `DisableWal` is the right durability for the
projection: it is rebuildable.

Writes take an internal mutex. Query methods only read RocksDB, which already
supports concurrent snapshots, so an in-flight fsync does not block `one` /
`select`. `append_batch` is the group-commit primitive: N events, one
`sync_data`.

`rebuild()` range-deletes the projection and reduces from event 0.
`verify()` checks the live projection against a fresh replay. The property
that matters is incremental execution == replay from zero.

The hop/Lua frontend uses the same runtime. [`dynpath`](../src/dynpath.rs)
is the untyped write algebra (a `Shape` plus a list of `Nav`s) so a Lua
reducer can `tx.put(["todos", tx.seq, "text"], …)` without a Rust
`Path<S>`. Browser VMs never get those bindings.

## Query language

Reads are a closed algebra ([`query`](../src/query.rs)): `Field`, `Key`,
`Index`, `All`, `Where`, `First`/`Last`/`Slice`, and a predicate language of
comparisons, `And`/`Or`/`Not`, and `Exists`. No callables. The wire form is a
tagged-array CBOR encoding shared with the Python frontend.

A path does not imply how to collect. Terminals are explicit: `one`, `select`,
`subtree`, `entries`, `project`. `explain` reports `POINT` / `BOUNDED` /
`SCAN` so a prefix scan cannot pretend to be a point get.

Once navigation enters a `Leaf`, remaining field/key steps run against the
decoded CBOR value. That is how `ROOT.evidence_by_id[id].payload` works when
the stored value is a dict.
