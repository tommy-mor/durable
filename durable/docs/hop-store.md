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

Two collecting navigators take arguments — predicates are data, not
closures:

```text
store.where(field)             keep entries whose field exists
store.where(field, v)          keep entries whose field == v
store.where(field, op, v)      op ∈ "==" "!=" "<" "<=" ">" ">="
store.slice(start, end)        a range of entries; nil bounds are open
```

```text
store(["tournaments", store.where("status", "live"), "name"])
store(["rooms", room, "messages", store.slice(50)])
```

Navigators are plain data (CBOR tag 27, `["nav", name]` or
`["nav", [name, args…]]`): they flow through packets, logs, and
diagnostics like any other value. The engine's `And`/`Or`/`Not`/path
predicates exist below this surface (durable's query algebra) and can be
surfaced the same way when an app needs them.

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

## Declaring the schema

The schema is a core block, not a runtime value:

```text
schema {
  todos: map<int, record { text: leaf, done: leaf }>,
  stats: record { created: sum, completed: sum },
  drafts: list<blob<map { role: string, text: string }>>
}
```

Keyspace types: `leaf | sum | blob<…> | map<T> | map<K,V> | list<T> |
deque<T> | record { … }`. Map keys are `any` by default (today's dynamic
scalars); naming one (`string | int | float | bool | bytes`) validates
writes and documents intent. `record` lives only here — a closed map
with field-id layout, never a value kind.

Inside `blob<…>` is the CBOR value language: `any | string | int |
float | bool | bytes | array<T> | map<T> | map<K,V> | map { closed
fields }`. `leaf` is `blob<any>` spelled short. Structs here are `map {
… }` (text keys, one opaque key); `record`/`list`/`sum`/`leaf` inside a
blob are compile errors that say so. `nil` is accepted for any blob
type, absent closed-map fields read as `nil`, and extras are tolerated —
a newer writer must not brick an older reader.

The layout is all the engine needs; the declaration additionally
validates reducer writes, failing fast at append: wrong map key kinds,
and blob values that don't match. Reads stay tolerant (they must be —
old tapes outlive new schemas), and the legacy `server let schema =
store.record(…)` form still binds, with no validation.

## The rest of the module

Field access on `store` also provides view sugar `store.items(path)` (map
entries as records with the key merged in as `id`), `store.has(path)`
(presence — see below), and `store.verify()` (replay the tape, compare
projections — the conformance check tests lean on). The legacy schema
form — the `store.record` / `store.map` / `store.list` / `store.deque` /
`store.leaf` / `store.sum` shape constructors as a `server let schema`
value — still binds, with the same layout and no write validation.

### Presence: `store.has(path)`

The query contract above says "missing is `nil`", and that is literally
true for **containers**: a missing map, record, list, or deque reads `nil`
— truthiness on a container read *is* presence. (The scan you were doing
anyway detects absence; no extra seek.) **Scalars keep their natural
defaults**: a missing leaf reads `nil`, a missing sum reads `0` — a
counter's empty state is zero, so ordinary counter reads need no guard.
Writes cannot help with presence either way: nested writes create missing parents by
key prefix (`files/x/wins.add(1)` creates a partial record for `x` without
ceremony), because blind-merge writes must never read first.

`store.has(path)` answers presence in the raw keyspace — one seek, no
materialization: true iff some non-null value exists at or under the path.
You need it exactly where truthiness can't reach: **sums** (was this
counter ever written, or is the 0 a default? — billing cares) and **O(1)
guards on large containers** (checking a 10k-entry map without reading it).
One honest limit: "never written" and "written empty" are the same
absence; if that distinction matters to the app, model it (a status leaf),
don't probe for it.

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
