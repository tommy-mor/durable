# Hop values — normative

The value model is the foundation of the rewrite: one representation in
memory, on the wire, and in the store. Encoding is CBOR everywhere;
debugging is an interpreter log flag, not a human-readable wire.

## Base kinds (closed — this list does not grow)

```text
nil
bool
int        i64
float      f64, NaN is not a value (rejected at construction and decode)
string     UTF-8
bytes
array      ordered, 0-based indexing at the language level
map        sorted by key; keys must be scalar (see below)
tagged     (tag: string, value) — the only extension mechanism
closure    VM-local only: not data, never crosses a boundary, never persists
```

`int` and `float` are distinct kinds: `1` and `1.0` are different values.
Scalar = nil, bool, int, float, string, bytes, or tagged-with-scalar-payload.

## Equality — structural, deterministic

- Values of different kinds are never equal (`1 ~= 1.0`).
- Scalars: by value. Floats numerically (`-0.0 == 0.0`; NaN cannot occur).
- Arrays/maps: deep, element-wise / entry-wise.
- Tagged: equal tags and equal payloads.
- Closures: identity. Comparing a closure for equality with data is `false`.

## Ordering — total over all data values

Needed for map keys and deterministic iteration (replay depends on it).
Order by kind rank first, then within kind:

```text
nil < bool < int < float < string < bytes < array < map < tagged
```

- bool: false < true. int/float: numeric. string/bytes: lexicographic.
- array: lexicographic element-wise. map: lexicographic over sorted entries.
- tagged: by tag, then payload.

The runtime comparison operators (`<`, `<=`, …) are narrower on purpose:
they accept two numbers (mixed int/float compares numerically) or two
strings; anything else is a runtime error. The total order above is the
*key order*, not the `<` operator.

## Truthiness

`nil` and `false` are falsy; every other value is truthy — including `0`,
`""`, and empty aggregates. (Lua's rule, now Hop's by decision.)

## Maps

- Keys must be scalar. Aggregate keys are a runtime error at insertion.
- Maps iterate in key order (the total order above). This makes `for k, v
  in pairs m` deterministic — a nondeterminism Lua had and Hop does not.
- Absent key reads as `nil`; assigning `nil` deletes the key. Absence and
  nil-value are indistinguishable, matching the wire and the store.

## Arrays

- 0-based, contiguous. Reading a negative index or past the end is `nil`;
  writing past `len` is a runtime error (no holes; appending at index
  `len` — or via `push` — is the only way to grow).
- `for i, v in xs` iterates 0..len-1 in order. (This is a deliberate break
  from the Lua era: the reference implementation is gone, and 0-based is
  the rule everywhere else Hop values live — CBOR arrays, the store's
  list indices, `Tx::seq`. Existing `.hop` apps are ported as part of the
  interpreter migration.)

## Numbers

- `+ - *` on two ints yields int (wrapping is a runtime error); any float
  operand promotes to float. `/` always yields float. `%` on ints yields
  int. `math.floor` returns int.
- String coercion is deterministic and engine-independent: ints render
  without a decimal point (`tostring(2)` = `"2"`, never `"2.0"`); floats
  render shortest-roundtrip.

## Tagged values

`tagged(tag, payload)` is the single extension mechanism: the tag names
the semantic, the payload is plain data. The runtime knows rich operations
for standard tags (vendored libraries supply *behavior*, never
*representation*); unknown tags still flow, compare, and persist — you
can carry a value you don't understand.

Starter registry:

```text
#inst      payload int — UTC epoch milliseconds. The tape stores only
           instants; timezone math is a view at the edges, never in a
           reducer (replay must not depend on tzdata).
#uuid      payload bytes (16)
```

Grow the registry on demand (`#duration`, `#decimal`, `#zoned` …), each
with a one-paragraph ruling. User/app tags are namespaced strings
(`#tournament/match`).

## Closures

- Capture **by value** at closure creation — a snapshot, consistent with
  copy-at-hop. Aliasing of aggregates still applies (the captured value
  may be a reference to a mutable array/map in this VM).
- Closures are not data: not serializable, not map keys, not store
  values. A closure in a hiccup attribute stays in its VM's handler
  table; rendered HTML calls back by id (hui semantics, unchanged).

## Copy-at-hop (promoted from accidental to specified)

Values crossing a placement boundary are copies taken at the moment of
the hop (`at` / `cast` / reply). Aliasing is a VM-local phenomenon. Two
casts of the same variable may carry different values if state mutated
between them.

## Encoding — CBOR, matching transport and store

One encoding for packets, store leaves, and the tape:

```text
nil/bool/int/float/string/bytes/array  native CBOR
map                                    CBOR map, entries in key order
                                       (canonical: equal values encode to
                                       equal bytes)
tagged                                 CBOR tag 27: 27(["<tag>", payload])
```

Tag 27 uniformly for now (one rule, no special cases); native CBOR tag
mappings (`#inst` → tag 1, `#uuid` → tag 37) are a later interop nicety.
Decoding rejects: NaN, non-scalar map keys, unknown CBOR tags other
than 27, and non-UTF-8 text. The wire is binary; readable dumps come from
the interpreter's log mode, which renders values in EDN-ish diagnostic
notation (`#inst 1756345678123`).
