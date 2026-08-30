# Hop semantics — the decisions already made

> **Historical.** Written during the Lua era; the Lua backend has since
> been removed. The native rewrite is described in [`hop-ir.md`](hop-ir.md)
> and [`hop-values.md`](hop-values.md).

Hop currently runs on Lua: Luau on the server, Lua 5.4 (wasmoon) in the
browser, `hopc` lowering `.hop` into pleasantly stupid code like
`rt.at("server", seg, { i = i })`. That is the right arrangement — Lua is
the executable reference implementation — but it carries a risk with a
name: **whatever Lua naturally does becomes the specification.** This
document is the antidote. It inventories every semantic decision Hop has
already made, says *where* each one is decided today, and sorts them:

- **specified** — deliberate; the implementation follows the intent.
- **inherited** — Lua's opinion, adopted by default. Needs a ruling:
  either promote it to specified or plan to diverge.
- **accidental** — fell out of implementation timing or a binding layer.
  These are the dangerous ones; each has already bitten or nearly bitten.

The motivating incident (2026-08-27): Hop had no ruling on what *absence*
means. So a binding layer ruled. mlua converts JSON `null` to a NULL
light userdata — which is **truthy** in Lua — so `if !m.winner` on an
unplayed tournament match silently inverted and every result report
no-oped. No error, no wire anomaly; the reducer just declined. The fix
(`hoprt/src/store.rs::to_lua`, now also used at packet delivery) chose a
semantic: absent = `nil` = falsy. This document exists so the next such
choice is made on purpose.

## One language, three value models

A Hop value today lives in three representations, and every boundary
crossing is a coercion with observable semantics:

```text
VM-local            wire                   store
Lua tables    ←→    JSON packets     ←→    CBOR values + Shape schema
(mlua/wasmoon)      (__send/__receive)     (durable: dynpath, query)
```

None of the three is Hop's value model; Hop's value model is currently
*the intersection that survives a round trip*. The wire is the most
honest of the three — it forcibly excludes functions, identity,
metatables, and cycles — so the spec should grow outward from the wire
and the store (where semantics are already Lua-independent) toward the
VM-local world (where Lua's opinions reign).

## The inventory

### Values that may cross a hop — **specified** (by force)

Only data crosses: nil, booleans, numbers, strings, and acyclic tables of
those. No functions, no identity, no metatables. Decided by
serialization at `__send` (`harness.rs`, `glue.js`, `serve.rs`); the
packet shapes in `hoprt.lua` are the de facto wire grammar:

```text
{ kind="call",  flow, to, hop, vars, origin, reply_to }
{ kind="cast",  flow, to, hop, vars, origin }
{ kind="reply", flow, to, value }
{ kind="error", flow, to, err }
```

Ruling to write down: this is the *definition* of a Hop transferable
value, not a limitation of the transport.

### Absence — **specified** (as of the NULL bug)

Absent = `nil` = falsy, in every representation: a missing store leaf, a
missing table key, an omitted `vars` entry, JSON `null`. All JSON→Lua
crossings go through `store::to_lua`, which maps `null` to real `nil`.
Two consequences already relied on: a Lua table field holding `nil`
vanishes from the wire (absence and nil-value are indistinguishable after
one hop), and `store.one` of a never-written path returns `nil`.

### Copy-at-hop — **accidental**, should be promoted

`let snapshot = todos; cast browsers { render(snapshot) }` works because
serialization happens at send time: `snapshot` is an alias of the live
server table right up until `rt.cast` builds the packet, then becomes an
immutable copy on every receiving VM. Nobody chose this; it fell out of
*when* `lua.from_value` runs. It is nevertheless the right semantic:
**values crossing a placement boundary are copies taken at the moment of
the hop; aliasing is a VM-local phenomenon.** Promote to specified, and
note the corollary: two casts of the same variable may carry different
values if the server mutated between them.

### Closures — **specified**

Closures never cross the wire. A lambda's segment 0 is a real closure on
the VM that built it (captures are Lua lexical scoping); only its marked
remainder ships, as a hop id plus the statically computed live set. hui
extends this to the DOM: a function-valued attribute is registered in a
VM-local handler table and rendered HTML calls back by id. Handler ids
are minted per render, released when that root re-renders, never reused.
These are real decisions about closure identity and lifetime; they are
currently documented only in `hui.lua` comments. Write them down.

### Ship sets (liveness) — **specified**

What crosses each hop is `refs(remainder) ∩ scope`, computed statically
by hopc. The tests assert exact ship sets (`delete_account` ships
`{} → {n} → {n, yes} → {msg}`); this is the strongest
already-conformance-shaped part of the suite.

### Truthiness — **inherited**

`if xs`, `if !m.winner`, `item.done && "done"` all use Lua truthiness:
only `nil` and `false` are falsy; `0` and `""` are truthy. Every app
already leans on this (chat guards empty input with `text == ""` because
`""` is truthy). This is a reasonable rule — but it is currently Lua's
rule, not Hop's. Ruling needed; recommendation: adopt it explicitly
(nil/false falsy, everything else truthy) so a future backend can't
disagree.

### Numbers — **inherited**, riskiest of the lot

A Hop number today is "whatever survives Lua ↔ JSON ↔ CBOR." Integers
and floats are distinct in all three layers, but the edges are engine-
dependent: Luau prints `2.0` as `2`, Lua 5.4 prints `2.0`; a float that
leaks into `"r" .. round .. "m" .. slot` would mint match id `"r2.0m1"`
on one engine and `"r2m1"` on another — a silent cross-VM divergence.
The tournament reducer defends with `math.floor`, which is convention,
not semantics. Ruling needed: what is a Hop number (i64 + f64? decimal?),
what arithmetic promotes, and what string coercion does. Until then:
integers on paths and in string coercions, floats only in leaf values.

### Arrays and maps — **inherited**

Hop inherits Lua's single-table duality: arrays are 1-based contiguous
integer keys; `for k, v in x` is `ipairs` (`pairs` opt-in by keyword);
there is no `#` in the surface language (helpers count by iteration).
The wire keeps arrays and objects distinct (JSON), and the store keeps
them distinct (Shape: list/deque vs map vs record) — so the *ambiguity
lives only in the VM layer*, and hui pays for it: fragment detection is
`type(node[1]) ~= "string"`, attrs detection is "table with no array
part." A first-principles Hop value model would make arrays, maps, and
records distinct as hiccup wants them to be. Rule later; for now the
store schema is the source of structural truth.

### Errors across hops — half **specified**, half **inherited**

Specified: an error in a remote segment unwinds the hop chain — each
side re-raises at its `rt.at` call site (`error(v, 0)`), so a browser
flow observes a server failure exactly where it hopped, and an unhandled
error surfaces at the flow origin. At-most-once `cast` deliberately has
no error channel.

Inherited: the error *value* is `tostring(res)` — a Lua string. Error
identity, structure, and any capability to catch-by-kind are lost at the
first boundary. Fine for the spike; a real ruling (structured error
values, at minimum `{ kind, message }`) is needed before error handling
grows syntax.

### Flows, ordering, identity — **specified**

- A flow is one coroutine-per-VM-visit; `at` suspends it, the reply
  resumes it. Replies resume the most recently suspended entry for that
  flow on that VM — LIFO, because hops nest like calls (`hoprt.lua`).
- Flow ids are `addr#n`. Identity rides the connection, not the packet:
  hopd overwrites claimed `origin`/`reply_to` with the session that owns
  the socket (`serve.rs`; `ws_smoke.rs` asserts forgery fails).
- `session()` means: own session on a browser; the flow's origin session
  in a server segment.
- Two identities, both socket-stamped, never client-claimed. A *session*
  is one tab (one WebSocket, minted per connection). A *user* is the
  durable identity behind it: a `hop_user` cookie minted by hopd's HTTP
  shell and read back from the WebSocket handshake — it survives reloads
  and is shared by every tab of one browser profile. `user()` mirrors
  `session()` (own user on a browser; the origin's user in a server
  segment), and `cast user(uid)` fans out to every connected tab of that
  user. hopd fires `on_connect(sid, user, profile)` /
  `on_disconnect(sid, user)` on the server VM — best-effort presence,
  not a transaction.
- Users can be *authenticated*. hopd serves `GET /auth/discord` (OAuth
  authorize redirect), `/auth/discord/callback` (code exchange +
  `/users/@me`), and `/auth/logout`. Success mints an HMAC-signed
  `hop_auth` cookie (`auth.rs`); the WS handshake prefers it over
  `hop_user`, so the uid becomes `d:<discord_id>` — one identity across
  browsers and devices. A bad signature falls back to anonymous
  (`oauth.rs` asserts the forgery path over real sockets). The
  `on_connect` profile is `{name, avatar, admin}` or nil; `admin` is
  membership in `HOP_ADMIN_DISCORD_IDS` (comma-separated env var),
  computed by hopd — apps gate mutating flows on it server-side.
- Delivery is FIFO per transport; `browsers` fan-out enumerates connected
  sessions at delivery time, at-most-once, no ordering across VMs; the
  `user:` fan-out enumerates that user's tabs the same way.
- Quiescence is observable: no suspended flows once the queue drains
  (`rt.quiescent`, asserted after every test scenario).

### The store — **specified** (the strongest layer)

durable's contract is Hop's persistence semantics and needs no Lua
ruling: the JSONL tape is the source of truth; the projection is
rebuildable; `Meta{seq, ts_ms}` is stamped at ingest (monotonic, never
from the event body); reducers are deterministic; `tx.peek` reads
committed state — an event's own writes are invisible to it (the
tournament's bracket generator builds its layout in locals for exactly
this reason); `store.verify()` is replay equality, asserted in every app
test. The reducer runs server-side only; a browser can only append.

## Conformance, not implementation

The test suite is already closer to a spec than the docs are, because its
best assertions never mention Lua: wire transcripts (`pipeline.rs`), ship
sets, forged-identity rejection over real sockets (`ws_smoke.rs`), replay
equality (`apps.rs`). Growing rule: **assert on packets, transcripts, and
store state — never on tables or VM internals.** Kept that way, the
harness is the reference oracle and the suite runs unchanged against a
future backend #2 (a Hop-native runtime, a WASM VM, anything that speaks
the packet grammar and the store contract).

## Rulings needed next (in order of blast radius)

1. **Numbers** — int/float model, path-key coercion, string coercion.
   Every layer touches this; divergence is silent.
2. **Error values** — structured errors before error-handling syntax.
3. **Truthiness** — one paragraph; adopt Lua's rule explicitly or don't.
4. **Arrays/maps/records as distinct kinds** — decides whether hui's
   heuristics are temporary or the actual data model.
5. **Copy-at-hop** — promote from accidental to specified (one line, but
   it defines what a "distributed value" is).

Lua is not currently holding Hop back. It starts holding Hop back at the
moment any of the five rulings above gets made by mlua, Luau, or wasmoon
instead of by this document.
