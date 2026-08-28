# Hop store — one function, paths all the way down (normative)

The store is a single callable: `store(path)`. What happens is decided by
the path, not by which method you called — the Specter lesson, taken
literally. There are no read verbs and no write verbs.

A path is an array. Its elements are:

- **scalars** — resolved against the app's declared schema: strings
  against a record are fields, anything against a map is a key, integers
  against a list are indices;
- **collecting navigators** (`#nav` tagged values) — the path now names
  many places;
- at most one **terminal navigator** (`#term`), which must come last —
  the path is now a mutation.

```text
store(["tournaments", tid, "status"])                      point query
store(["tournaments", store.keys])                         collecting query
store(["tournaments", tid, "status", store.set("live")])   mutation
```

## Queries

A path with no terminal is a query. A point path returns the value at
that location — a leaf's value, or the materialized subtree of a record,
map, or list. Missing is `nil` (absence and nil-value are one thing,
matching the value model). A path containing a collecting navigator
returns the array of every focus.

```text
store.all       every entry of a map / element of a list
store.vals      values of a map
store.keys      keys of a map
store.entries   [key, value] pairs
store.first     first element
store.last      last element
```

Navigators are plain data (CBOR tag 27, `["nav", name]`): they flow
through packets, logs, and diagnostics like any other value.

The engine's `Where` predicates and `Slice` exist below this surface
(durable's query algebra) and are future hop surface — predicates need a
data syntax, not closures.

## Mutations

A path ending in a terminal navigator reifies a write:

```text
store.set(v)    write v here; at a record path, expands per field
store.add(n)    blind merge on a Sum (commutative, replay-safe)
store.push(v)   append to a list/deque
store.del       delete: a leaf exactly, a collection or record by range
```

One `del` covers what used to be `delete` and `clear`: the schema shape
under the path decides. Collecting navigators are rejected on the write
side — a mutation names exactly one place.

## Context is capability

The same `store` global exists everywhere; where you are decides what a
path may do:

- **In `fn reduce(event)`** — queries read *committed* state, terminals
  are legal. There is no `tx` handle: the reducer is the transaction. The
  event's sequence number is on the event itself (`event.seq`, injected
  by the runtime).
- **In a server segment** — queries read the projection; a terminal
  navigator is an error. State changes by appending:
  `store.append(event)` (returns the seq).

```text
fn reduce(event) {
  if event.type == "add" {
    store(["todos", event.seq, store.set({ text = event.text, done = false })]);
    store(["stats", "created", store.add(1)]);
  }
}
```

## The rest of the module

Field access on `store` also provides the schema shape constructors
(`store.record`, `store.map`, `store.list`, `store.deque`, `store.leaf`,
`store.sum`), view sugar `store.items(path)` (map entries as records with
the key merged in as `id`), and `store.verify()` (replay the tape,
compare projections — the conformance check tests lean on).

## The reducer reads committed state

Queries inside `reduce` see the projection as of the *previous* event: a
reducer's own writes are invisible until its event commits. This is a
feature — replay depends on reducers being functions of (committed
state, event) — but it shapes how you write them:

- Deriving a lot of state at once (laying out a whole tournament
  bracket): build it in a local map, then write it once.
- Incremental updates that cascade (advancing a winner into a match that
  might auto-resolve): read only keys that earlier events committed, and
  carry this event's own placements as arguments.

tournament.hop demonstrates both patterns.
