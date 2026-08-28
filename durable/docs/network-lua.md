# network-aware Lua — linear placement marks over mlua

> Companion: [`hop-semantics.md`](hop-semantics.md) inventories the
> semantic decisions Hop has made so far — specified vs inherited-from-Lua
> vs accidental — so Lua stays the reference implementation rather than
> becoming the spec.

*A brace-syntax layer that compiles to Lua. Function bodies may contain
placement marks; execution hops between browser and server mid-body; the
function boundary hops home. The runtime is Luau embedded in the Rust host
via mlua on the server, and Luau-in-WASM in the browser.*

Supersedes the cljrs design (see git history for `network-cljrs.md`). The
semantics survived the language change; the implementation got simpler.

## The stack

```text
┌ browser ─────────────────┐        ┌ server ──────────────────────┐
│ Luau VM (WASM)           │  wire  │ Rust host                    │
│ same compiled program    │ ◄────► │  ├ mlua/Luau (same program)  │
│ DOM bindings             │  WS    │  ├ WebSocket + static serve  │
└──────────────────────────┘        │  └ durable Runtime (JSONL + projection) │
                                    └──────────────────────────────┘
```

- **One dialect both sides.** Luau runs natively (with codegen) on the
  server and compiles to WASM for the browser. No LuaJIT — it can't run in
  WASM, and Luau's interpreter + native codegen covers the server.
- **Sandboxed by design.** Luau's restricted stdlib is a feature: hop
  bodies reachable from clients run in a VM built for untrusted code.
- **The syntax layer compiles to plain Luau.** Curly braces, no whitespace
  sensitivity, no required type annotations, MoonScript-level sugar, richer
  data literals (for hiccup-style structures). Surface details deferred;
  everything below is about what the compiler *does*, which is settled.

## Semantics: linear placement, function-boundary returns

A function body is a linear sequence of statements, optionally punctuated by
**placement marks**: `server!()` and `browser!()`. A mark moves execution of
everything after it to that side.

```text
fn rename_notif(idx, title) {
  browser!();
  dom.patch(spinner(true));      // runs in the tab
  server!();
  notifs.rename(idx, title);     // runs in the app
  let n = notifs.count();
  browser!();
  dom.patch(spinner(false));     // back in the tab
  return n;
}
```

The rules, each one a deliberate decision:

1. **Marks are statements; granularity is linear.** No dataflow, no emit
   semantics. Code between marks is ordinary imperative Luau and runs where
   the last mark put it.
2. **The function boundary is the implicit return hop.** Whatever side the
   body ends on, `return` delivers the value back to the *caller's* side.
   Callers see a plain function: call, get value. Placement is a private
   detail of the callee. (`rename_notif` above returns `n` to the browser
   code that called it — the trailing `browser!()` before the return isn't
   what brings the value home; it's there because `dom.patch` must run in
   the tab.)
3. **Locals flow across marks with normal scoping.** `idx`, `title`, `n`
   are just variables. What actually crosses each hop is computed at
   compile time: the locals live in the remainder of the body. Live-vars
   analysis is static, over the layer's own AST — no VM introspection
   (Luau's sandbox forbids `debug.getupvalue` anyway, so this is forced,
   and good).
4. **Unmarked functions are location-agnostic.** No marks → normal Luau
   function, runs wherever it's called, on either side. Library code stays
   portable; only choreographed functions mention the network.
5. **`browser!()` means the flow's origin session.** Every flow starts
   somewhere: a DOM event starts it in a tab (that tab is the origin), a
   server timer starts it with *no* origin — `browser!()` there is a
   compile error. Statically checkable because marks are syntax.
6. **Exceptions propagate through hops.** An error in a server segment
   unwinds across the wire and is catchable at the browser call site with a
   normal `try`/`catch` (or `pcall` at the Lua level). Disconnection is an
   exception like any other.
7. **Fan-out is not linear.** You cannot continue a body on every connected
   tab and then hop home. Broadcast and direct sends are a separate,
   value-less form:

```text
cast browsers { dom.patch(stroke_svg(from, to, color)); }
cast session(sid) { dom.flash(color); }
cast server { log.stroke(from, to); }        // fire-and-forget upstream
```

`server { ... }` as an *expression block* (the original sugar idea) is not a
separate feature: it's a nullary marked function, inlined. Block form and
linear marks compile identically.

## What the compiler emits

Marks split the body into segments; each segment becomes a Luau closure
registered under a stable hop id (chunk name + mark index) in a table both
sides build at load time. A hop is one WebSocket/queue message:

```lua
-- rename_notif, compiled (shape, not literal output)
local function rename_notif__seg2(vars)            -- server side
  notifs.rename(vars.idx, vars.title)
  local n = notifs.count()
  return hop("rename_notif:3", { n = n })          -- → origin browser
end

local function rename_notif__seg3(vars)            -- browser side
  dom.patch(spinner(false))
  return vars.n                                    -- function boundary:
end                                                -- value → caller's side

hops["rename_notif:2"] = rename_notif__seg2
hops["rename_notif:3"] = rename_notif__seg3
```

Wire packets carry locations and data, never code:

```lua
{ flow = "…uuid…", hop = "rename_notif:2", seq = 4,
  vars = { idx = 7, title = "Q3 report" } }
```

- **Flows are coroutines.** Each entry point (event handler, timer) runs as
  a coroutine; a hop yields it; the reply resumes it. Blocking-looking code,
  nothing blocks. This is stock Lua machinery — no fork of anything.
- **Tail-hop optimization.** Naively, a body that hops browser → server →
  browser returns by unwinding through each segment. When a mark is in tail
  position, the compiler routes the reply straight to the caller's side —
  one return hop regardless of segment count, the hop-level analog of tail
  calls.
- **Ordering** is per-pair FIFO, inherited from the WebSocket. Two casts
  from the same sender to the same target arrive in order; nothing is
  promised across senders.
- **Delivery to tabs is at-most-once.** Sessions are ephemeral. `cast
  browsers` enumerates connected sessions at emit time; a reply that can't
  arrive raises a disconnect error in the suspended flow.
- Marks inside loops are legal (the closure split handles it) and each
  iteration pays two hops — chatty code is *visible* in the syntax, which
  is the point of marks.

## Security is still the hop table

Every segment that runs on the server and is reachable from a
client-originated flow is an endpoint taking `vars` as input. The set is
static — `hopc routes` can print it. Vars are data at a boundary: validated,
never evaluated. Session identity rides the connection (`session()` in
server segments), never the packet. Luau's sandbox bounds what a segment
can touch even after a validation mistake.

## Why this language shape (decisions of record)

- **Not cljrs/Clojure**: parens ergonomics, single-maintainer runtime that
  would need interpreter surgery for suspension, tiny ecosystem, and the
  Rama resemblance worry. Lua's coroutines give suspension as a library;
  Luau gives speed, WASM, sandboxing, and enormous LLM familiarity (Roblox
  corpus).
- **Not LuaJIT fork**: no WASM story, and nothing needed forking.
- **Not MoonScript itself**: dormant since ~2015 (Yuescript is the live
  fork); whitespace sensitivity is the one syntax class LLMs reliably
  fumble. Hence: MoonScript's *sugar density*, Rust-ish braces, explicit
  delimiters.
- **Not TypeScript**: async coloring would leak the boundary into every
  signature, and that lane (Meteor then, Convex/tRPC now) is crowded.
  The Lua lane is empty.
- **Ecosystem honesty**: Lua's library bench is thin. The Rust host is the
  battery pack — HTTP, WebSockets, crypto, and storage are platform, and
  Rust crates surface through mlua as opaque handles.

## The spike: runtime before compiler

[`hoprt/`](../../hoprt/) (workspace crate, `cargo run -p hoprt`) proves the
runtime with zero syntax: one server Luau VM and two browser Luau VMs in one
Rust process (mlua), connected by a queue that carries only serialized
packets. [`hoprt/lua/app.lua`](../../hoprt/lua/app.lua) is the
*hand-compiled* form of the `.hop` examples — exactly what the compiler will
emit, written by hand first.

The run demonstrates, in one ordered transcript: four flows from one tab
interleaving (all four call packets depart before any reply is processed —
coroutine suspension, nothing blocks); values and errors crossing as data
with a server exception caught by an ordinary `pcall` at the browser call
site; nested hops (browser → server → browser confirm dialog → server) with
replies routed by the per-flow LIFO stack; and a cast fanning out to every
session with server-stamped authority. It ends with a quiescence check —
every VM must have zero suspended flows once the queue drains.

Swapping the in-process queue for a WebSocket changes no semantics; that is
the claim the spike exists to test.

**hopc v0 exists and closes the loop.** `hoprt/src/compiler.rs` compiles a
v0 subset of the brace layer — `fn`, `let`, `if`/`else`, assignment,
placement marks, nested `cast` blocks, `spawn`, `server let` globals — into
exactly the segment-closure form above. Liveness is computed statically
(the variables the remainder references, intersected with scope), marks
inside branches are rejected at compile time, and the same demo runs
compiled from source:

```text
cargo run -p hoprt -- hoprt/hop/demo.hop    # .hop → hopc → cluster
cargo run -p hoprt                          # hand-compiled app.lua
cargo test -p hoprt                         # pipeline + liveness assertions
```

The wire log of the compiled run shows the analysis working: the
`delete_account` chain ships `{} → {n} → {n, yes} → {msg}` across its four
hops, and the phase-2 tail of `reply` packets unwinding segment by segment
is the naive return chain — the tail-hop optimization deferred above,
visible in a transcript.

## UI as data: hui, and closures in attributes

The first todo app built its HTML by string concatenation, with
`onclick="hopFire('toggle_todo', 1)"` embedded in the strings — stringly
typed UI, the exact thing this project exists to remove. The fix has two
halves, and the second is the interesting one.

**hui** (`hoprt/lua/hui.lua`) is a hiccup renderer, loaded into every VM
like the runtime itself. A node is data — `[:li, { class = "done" },
text]` — with keyword literals (`:li` is just a string), attribute maps,
and fragments (a list built with `table.insert` splices in place). Nothing
novel; it exists so views are values that plain functions like
`todo_view(items)` can return, on whichever VM happens to be rendering.

**Function-valued attributes are closures, and closures may hop.** This
falls out of the compilation model rather than being a feature bolted on:
segments were already closures, so a lambda in an attribute compiles the
same way any marked function body does — its segment 0 is emitted *inline*
as a real Lua closure, and the marked remainder registers under
`enclosing:lN:i` hop ids. The whole todo item, view and behavior:

```text
[:li, {
  class = item.done && "done",
  onclick = fn(e) {
    server!();                       // ships {i} — the closure's capture
    todos[i].done = !todos[i].done;
    let snapshot = todos;
    cast browsers { hui.render("#todos", todo_view(snapshot)); }
  }
}, item.text]
```

The division of labor is exact. The loop variable `i` is captured by Lua's
own lexical scoping, in the browser VM where the lambda was built — the
closure itself never crosses the wire (hui registers it in a local handler
table and rendered HTML calls back by id). Only when the handler *hops*
does anything ship, and then it's the usual liveness set: `{ i = i }`,
computed statically. The generated attribute value is two lines:

```text
onclick = function(e)
  return rt.at("server", "todo_view:l1:1", { i = i })
end
```

Handler ids are minted per render and released when that root re-renders;
each activation runs as a flow, so handlers get the full hop machinery —
suspension, replies, error propagation to the click site.

## Examples

In [`../examples/netlua/`](../examples/netlua/), written in the brace layer
(surface syntax illustrative, semantics binding):

- [`handle_check.hop`](../examples/netlua/handle_check.hop) — one marked
  function; live-vars per hop annotated; error handling; a loop with marks.
- [`whiteboard.hop`](../examples/netlua/whiteboard.hop) — multiplayer:
  linear marks for the authoritative path, `cast` for fan-out, direct
  session sends.

And running for real: `hoprt/hop/todo.hop` is the multiplayer todo app —
hiccup views, marked lambdas as click handlers, served to actual browsers
by `hopd` (`cargo run -p hoprt --bin hopd -- hoprt/hop/todo.hop`).

## Storage: the durable store is a server library

A hop app that wants to *keep* state declares a schema and a reducer.
`hopd` opens a [`Runtime`](../src/runtime.rs) on the server VM only:
JSONL log first, RocksDB projection rebuildable from it. Browser VMs
never see the store. The only legal write is `store.append(event)` from
a server segment — the hop spelling of ramalite's `(server! …)`.

```text
server let schema = store.record([
  ["todos", store.map(store.record([
    ["text", store.leaf],
    ["done", store.leaf]
  ]))],
  ["stats", store.record([
    ["created", store.sum],
    ["completed", store.sum]
  ])]
]);

fn reduce(tx, event) {
  if event.type == "add" {
    tx.put(["todos", tx.seq], { text = event.text, done = false });
    tx.add(["stats", "created"], 1);
  }
}

fn add_todo(text) {
  server!();
  store.append({ type = "add", text = text });
  let snapshot = store.items(["todos"]);
  cast browsers { hui.render("#todos", todo_view(snapshot)); }
}
```

What this is, mechanically:

- **`schema`** is data. `store.record` / `store.map` / `store.leaf` /
  `store.sum` / `store.list` / `store.deque` are constructors; field
  order is the array order, the same declaration-order ids the Rust
  `#[derive(Durable)]` layout uses.
- **`fn reduce(tx, event)`** is a Lua reducer. `tx.put` / `tx.add` /
  `tx.delete` / `tx.push` / `tx.clear` emit reified writes; `tx.peek`
  reads committed state; `tx.seq` is the log index (server-assigned
  ids — never take them from the browser). The Rust host lowers those
  writes through the untyped path algebra
  ([`dynpath`](../src/dynpath.rs)) and commits them atomically with
  the applied offset.
- **`store.append` / `store.one` / `store.entries` / `store.items` /
  `store.subtree` / `store.explain` / `store.rebuild` / `store.verify`**
  are the ramalite surface, now hanging off the hop server VM.
- **Restart** is `catch_up`. The tape is `hop-data/log.jsonl`;
  `hopd --data <dir>` sets the directory.

`hoprt/hop/todo.hop` is the running proof: the same multiplayer todo
app, but a crash or a hopd restart no longer empties the board, and
`store.verify()` still holds incremental == replay from zero.

The same store is what the larger apps sit on — they exist to see
whether hop + the tape scale past a todo list:

- [`hoprt/hop/ranking.hop`](../../hoprt/hop/ranking.hop) — pairwise
  votes into a `Sum` edge graph, plus decay as an event (replay
  reproduces the 0.9 scale).
- [`hoprt/hop/microblog.hop`](../../hoprt/hop/microblog.hop) —
  follows and fan-out-on-write home timelines. The reducer peeks
  the author's follower list and `tx.push`es the new post id onto
  every inbox.
- [`hoprt/hop/chat.hop`](../../hoprt/hop/chat.hop) — rooms as a
  nested `Map → List`. Switching rooms is a query; a send is one
  `ListPush` under that prefix.

Each one is served by `hopd` the same way (`cargo run -p hoprt
--bin hopd -- hoprt/hop/<app>.hop`). Simulated-cluster coverage
lives in `hoprt/tests/apps.rs`; two-tab browser coverage is
`hoprt/e2e` (Playwright against a real `hopd`).

Reactive queries (write-key ∩ subscribed range → `cast browsers`) are
still a library on top of this, not a language feature. hopc checking
paths against the declared schema is the next compiler pass — the
runtime already speaks the same navs.

## Later, not now

- **Reactive invalidation**: a server watcher that `cast browsers`
  only the queries whose key ranges intersect the event's write set.
- **hopc path check**: typed paths against the declared schema, so a
  mistyped `store.one({"todoes", id})` is a compile error.
- **Syntax**: deferred on purpose. Requirements recorded: braces, no
  whitespace sensitivity, no mandatory type annotations, MoonScript-level
  sugar. (Hiccup data literals: done, see above.)
- **More locations**: web workers and multiple app processes give the
  target slot something to select over again; marks don't change.
- **hui diffing**: today every render replaces innerHTML under the
  selector. Fine at this scale; keyed diffing is a renderer upgrade, not a
  semantics change.
