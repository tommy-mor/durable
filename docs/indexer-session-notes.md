# Findings from the first indexer.hop session (2026-10-05)

*Corrected after reading the durable source. An earlier draft of this note
claimed "reduce writes apply partially on error." That was wrong; the real
mechanics are subtler and more interesting — see below.*

## What actually happened (source-verified)

**1. Reduce IS atomic per event.** `Runtime::apply_record` builds one `Tx`
batch, runs the app reducer, and commits the event's writes plus the applied
offset in a single RocksDB batch (`runtime.rs:440-465`). A reducer error
drops the whole batch — nothing applies. The design doc's promise ("a crash
before commit simply replays against the previous state") is real code.

**2. Writes are schema-resolved, not data-resolved — they create missing parents.**
`dynpath::add` (`dynpath.rs:313`) resolves the path against the *Shape*, not
the stored data, and writes by key prefix. RocksDB doesn't know whether a
parent record "exists." So `store(["repos", r, "files", missing, "wins",
store.add(1)])` happily creates a partial record (`wins` present, `lines`
absent). No error, ever, at this layer.

**3. Missing reads are shape-dependent — and records read truthy.**
`query.rs`: a missing leaf reads `Null`; a missing sum reads `0`; a missing
map/record reads an **empty map** (`query.rs:361`: "Prefer `one` for scalars
so a missing leaf is Null, not an empty map" — the asymmetry is deliberate).
An empty map is truthy in hop. So an existence check written JavaScript-style
(`let w = store([...files, key]); if w …`) can never work: the answer is
always "yes, an empty record."

**4. `store.set` on a record replaces the whole record.** My `add_file` did
`set({lines, wins: 0})`, which silently reset wins that a premature judgment
had been created early. Combined with (2) and (3), this produced the "partial
application" illusion: `votes = 2` while `wins = 1`. Fully explained, no
atomicity violation.

## The bug is an impedance mismatch — and the missing abstraction was presence

durable's store is a **CRDT-flavored, schema-typed, parent-creating keyspace**:
blind-merge sums, protobuf-like zero-value reads, records-as-key-prefixes.
hop exposes it with **JS-dict intuitions** (nil checks, truthiness). I wrote
JavaScript against a protobuf.

**Shipped fix:** the missing abstraction arrived in two pieces. First,
`store.has(path)` in hop (`NativeId::StoreHas`), backed by
`durable::query::has` / `Runtime::has`, a raw-keyspace probe (one seek, no
materialization) pinned by `tests/query.rs::has_probes_presence_in_the_raw_keyspace`.
Second — and this is the root fix — **missing containers now read `nil`**
(`materialize` in `query.rs`): a missing map/record/list/deque answers nil
instead of an empty costume, so ordinary truthiness *is* presence for
containers and app code stays guard-free. Absence is detected while
reading; nothing pays an extra seek. Scalars keep their defaults: missing
leaf → nil, missing sum → 0 (a counter's empty state is zero; forcing
guards there broke todo.hop's unguarded `completed` read on first render —
the pipeline suite caught it, and it proved the boilerplate worry real).
`store.has` remains for sum presence (billing) and O(1) guards on large
containers. `hop-store.md` "Presence" documents the contract.

## Residuals

- A reducer error leaves the event on the tape but the projection behind;
  replay hits the same error. Poisoned tapes are possible in principle; the
  append path surfaces the error (`Error::Reducer`), so hopd can at least
  refuse loudly. Worth checking what hopd does with an `Error::Reducer` from
  `Runtime::append` — refuse the client? log and continue?
- One observation from the session (a browser read of "0 judgments" in the
  pre-validation build) does not reconcile with these mechanics and was
  most likely a stale-render misread on my part; the tape-verified run
  reconciles fully. Noted for honesty.

## Pleasant surprises

- `store.items()` snapshots carry `id` and cross `cast browsers` cleanly.
- Error unwinding across hops works exactly as the README claims (sim demo).
- The sim harness quiescence check caught nothing all session — solid.
