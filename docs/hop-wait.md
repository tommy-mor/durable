# hop wait — how the world reaches the other tabs

*Status: SHIPPED (single-path `store.wait`), 2026-10-11. The design below
landed as: `durable::watch` (the shipped bridge), `store.wait(path)` with
prefix-overlap waking, `spawn` handles + `cancel` / `cancel_origin`, tab
close ending flows, and the read-seq riding every hop and reply. Still
open: general `wait([...])`, the morph-race rule, the dev lint, Rust
reducers. Details at the bottom.*

*Original landing note (design, not spec): written after a long night of
going around the problem from every side. This records the path, not just
the destination, because most of the value was in learning which
reasonable-looking ideas are wrong for hop and why.*

## The question

When one person's flow changes the world, how does everyone else's screen
find out?

The admin bans bob. The admin's own screen is easy: the ban flow is the
admin's story, and it can say "show bob as banned" because it just did the
banning. But bob also has to vanish from every *other* user's friends list,
in tabs whose owners clicked nothing. Today every app in `hoprt/hop/`
answers this by hand, and they answer it four different ways:

- **push one snapshot to everyone** (todo, ranking, guardian2,
  file_browser): read, render once, `cast browsers { render(...) }`. Breaks
  the moment views differ per tab.
- **poke, then everyone pulls** (chat, microblog, tournament):
  `cast browsers { refresh(); }` and each tab hops back for its own view.
  N round trips per write; microblog needed a careful comment about whose
  `session()` is whose.
- **per-user push** (agent): `paint_user` / `paint_all` loop over a
  `views` map kept on the tape.
- **just me** (`open_room`, `open_t`): `cast session(me)`.

The same pain shows up as tab state with no home: hidden DOM inputs
(`#current-room`, `#current-tid`), navigation written to the tape as events
(file_browser's `cwd`, agent's `views`), and scope baked into browser
closures.

## What hop is (the constraint every idea was judged against)

Three convictions came into focus, mostly by rejecting ideas that violated
them:

1. **Flows read like user stories, not RPC.** `delete_account` in
   `demo.hop` is the model: the server counts your stuff, you're asked if
   you're sure, it deletes, you're told what happened. Marks are scene
   changes, read top to bottom.
2. **HTML is the home of the program.** A lambda in an attribute —
   `onclick = fn(e) { server!(); ... }` — is a story that is born inside a
   button. Behavior lives where it is attached (HyperCard's lesson).
3. **Hop is imperative.** Nothing should happen that the code didn't say.
   No hidden listeners, no magic re-renders.

And the foundation that was never in question: **the tape is the asset.**
Events are facts, the reducer turns facts into truth, replay rebuilds it.

## The long way around

Each of these was a reasonable design. Each was rejected for a specific
reason, and the reasons are the useful part.

**Server-side subscriptions with declared views** (an earlier design doc):
`subscribe [path] as name => view(...)`. Sound storage idea, but it moves
the repaint away from the story into a declaration, and asks the programmer
to keep a second registry in sync with what views actually read.

**Elm-style loop per tab** — one loop, `next_event()`, match on inputs,
re-render the whole state. Clean, and Phoenix LiveView proves the shape
works. Rejected because it rips behavior *out* of the HTML: clicks become
messages dispatched in a central loop instead of stories attached to their
buttons. Hop's lambdas-in-attributes are the more distinctive idea.

**Placement marks inside the hiccup tree** (Electric / React Server
Components style): `[:body server!() let isAdmin = store(...) browser!() ...]`.
Each server island's read set would be its subscription. Rejected because
declarative templates belong to expression languages; hop is imperative.
Targeting the DOM by selector (`dom.morph("#admin", panel)`) fits better.

**Two handlers per event** — `event ban { reduce {...} then {...} }`, where
`then` repaints after commit. It deletes duplicated paint code and gets
read-your-writes for free. Rejected because it reads like a rulebook
("when X, do Y"), not a story. Still contains one rule worth keeping:
**reducers replay; reactions never do.**

**`browser!(target)`** — let the actor's story address bystanders
directly: `browser!(admin) ... browser!(ALL) dom("#friends").remove(bob)`.
Story-shaped, explicit, auditable. It survives as the way to deliver
*news* (toasts, "you've been banned"). But for *state* it couples every
flow to every screen: add a screen that shows users and you must edit
every flow that changes users. That coupling is the dependency graph
between views and data — which the store already has.

**Polling with dirty checks** — a 60fps loop asking "did anything under
`users` change?", made cheap with per-prefix versions or a Bloom filter
over written prefixes (filters merge with bitwise OR, so a frame of writes
is one fixed-size filter). It works, and the Bloom filter's one-sided error
is exactly right for repaints. Rejected because a fullstack runtime should
*push*: the user's behavior is what gets pushed to, not what does the
pushing.

**A third occupant — the courier.** Tab (follows the user), reducer
(events to truth), courier (truth to tabs). The courier sees each commit's
write batch and pushes to tabs showing something that changed. This is the
design `durable/docs/ramalite.md` already describes:

```text
append → reduce → commit {write keys} → ∩ {subscribed query ranges}
       → re-run only the hit queries → patch only those browsers
```

Arriving back at our own earlier idea from a completely different direction
was the strongest evidence that it was right.

**`live` views with auto-registered reads** — split code into stories (run
once) and live views (re-run forever), with every `store()` call inside a
live view silently registering a listener. The run-once / run-forever
distinction is real. Rejected because hidden registration is exactly the
magic hop avoids, and `live` as syntax is declarative creeping back in.

## What we landed on

One new verb. **A story can wait for the world to change.**

```text
fn chat_panel(room) {
  loop {
    server!();
    let ms = store(["rooms", room, "messages"]);
    browser!();
    dom.morph("#messages", messages_view(ms));
    store.wait(["rooms", room, "messages"]);
  }
}
```

Show the messages, wait until they change, show them again. It is still a
story. Nothing listens that the code didn't name.

`store.wait(path)` parks the flow the same way a hop, an LLM call, or a
human click already parks it, and the courier's job shrinks to *waking
parked flows* whose watched paths overlap a commit. Users, servers, models,
and data all become things a story can wait on.

### Waiting on more than one thing

Usually, don't. A lambda in an attribute is already its own story, so the
panel never waits on clicks: the send button's story appends, the append
wakes the panel. Two small flows cooperating through the store.

When one story's next step genuinely depends on which happens first,
`wait` takes a list of waitables (tagged values, like store navigators)
and returns whichever fired:

```text
let ev = wait([
  dom.click("#approve"),
  dom.click("#deny"),
  store.changed(["threads", tid, "pending"])   // another tab decided first
]);
match ev { click { target } => { ... } changed {} => { ... } }
```

That is agent.hop's approval gate, which today has no answer for two tabs
deciding at once.

### Waiting from the browser

Prefixes depend on the schema, which lives on the server. The browser never
computes them. A browser-side `store.wait(path)` ships the *path* (plain
data) to the server, which resolves it, checks the tab may read it, and
registers the parked flow. One message to register, one to wake.

The commit point of the flow's last `store()` read travels with the flow
invisibly (like `origin` does), so a write that lands while the browser is
rendering — between the read and the wait — wakes the flow immediately
instead of slipping through.

### Lifecycle

`spawn chat_panel(room)` returns a handle. Switching rooms is a story that
cancels the old panel and spawns a new one. Closing a tab ends its flows.
Tab state stops needing a home: "which room am I in" is the argument of
the panel flow currently running.

### Two channels, two jobs

- **State** — what *is*: the friends list, counts, rankings. A flow that
  shows it waits on it. Missing an update would leave the screen *wrong*.
- **News** — what *happened*: "you banned bob", "you've been banned", a
  streaming LLM token. Pushed one-way by the story that knows it
  (`cast`, or the `browser!(target)` idea). Missing it means you didn't
  hear the news, not that the screen is wrong.

## The load-bearing bridge: prefix overlap

Everything above rests on one claim: a commit's write set and a watched
path's key range overlap whenever the write could change what that path
reads. That was tested, not assumed — `durable/tests/invalidation_lab.rs`
runs real writes against real RocksDB and checks every strategy against an
oracle that re-runs each query before and after.

Results (14 named edge cases + 20,000 randomized read/write pairs):

| strategy | missed repaints |
|---|---|
| register the watched path's whole subtree, overlap checked **both directions** | **0** |
| same, but naive one-way check ("is the written key under my prefix?") | 130 of 2,236 real changes (~6%) |
| register only the rows the view showed | misses `where`, `first`, `slice`, empty-then-created containers, and precise pushes |

An independent suite (written by another model against the same crate)
reached the same verdict, with a simpler check worth adopting:
`a.starts_with(b) || b.starts_with(a)`. Two prefix ranges either nest or
are disjoint, so that *is* interval overlap. It also stress-tested the
encoding itself: 17,000 record fields crossing every varint width, field
id 0 vs map key 0 vs list index 0, hostile byte keys. Length-prefixed
segments keep siblings disjoint in every case.

Why it is sound, not just tested: durable's keys nest exactly the way the
data nests. The schema *is* the dependency graph; prefix overlap just reads
it. Anything else (hand-written event→view maps, dirty flags, version
counters) rebuilds a second copy of information the keyspace already holds.

### The rules the bridge needs

1. **Watch whole subtrees, never the rows you saw.** Stop at the first
   collecting step (`where`, `first`, `slice`, `keys`), a deque index, or
   a leaf blob's interior, and watch everything under that point.
2. **Check overlap in both directions.** Deleting `users/bob` or clearing
   `users` is a write *above* a watcher of `users/bob/banned`.
3. **Register atomically with the read.** A commit between the read and the
   registration is checked against nobody and the screen stays stale. hopd's
   main thread owns the VM and every commit, and the read-point carried with
   the flow closes the gap across a browser render.
4. **Only RocksDB is visible.** `server let` globals (demo.hop's `members`),
   `fs.*`, `http.*`, and env reads never wake anyone.

### What it costs

Never wrong, sometimes wasteful: ~16% of unchanged views got flagged in the
randomized run (pessimistic: tiny keyspace). Sources: a leaf blob is one key,
`add(0)` and same-value writes still write, a `keys` view wakes on value
changes, and any `where`/`slice` watches its whole collection. Comparing a
re-run's result to the last one sent removes the network cost. Precision,
when it matters, comes from the reducer maintaining an index (`banned[uid]`)
so the panel watches a narrow prefix — not from a smarter bridge. The bridge
stays dumb and correct; precision lives in the schema.

This also answered a separate worry about WHERE clauses in paths: they are
honest convenience on small maps and a smell on large ones, for exactly this
reason.

## Found along the way

These came out of reading every app in `hoprt/hop/` and are independent of
the design above.

- **Early `return` across a hop is broken.** `fn f(c) { if c { server!();
  return 1; } return 2; }` returns 2: the far piece's reply is popped and the
  parent resumes its continuation. In a loop it keeps hopping after the
  return. Fix: replies from pieces that can return carry a `ret` flag, and
  each mark checks it (forwarding up through nested pieces). Reproduced with
  a probe test; fixed on `cursor/early-return-770a` (PR #8) with a 4-test
  `hoprt/tests/early_return.rs` suite — this note predates the fix.
- **Identity should be stamped on events at ingest, like `seq`.** `append`
  stamps `seq` and `ts_ms` but not who. Ranking's votes carry no voter and
  take scope/winner/loser from the client. Agent passes `uid` from browser
  closures, and `approve(tid)` will run the pending bash command for any
  thread from any connected client. whiteboard.hop already stated the rule:
  identity comes from the connection, never from a var the client sent.
- **Flows append intents; reducers make decisions.** `new_thread` reads
  `next` in a flow, then appends `thread { tid }` — two clicks mint one id.
  todo and tournament already use `event.seq`.
- **The tape is collecting machine noise.** guardian2 appends a `cursor`
  event every 150ms (~570k a day), alongside `play`/`pause`/`err`/
  `set_entries`/`open`. Facts and runtime state want different homes; the
  wait design gives tab state one (the args of running flows).
- **Validation lives in reducers as silent no-ops** (tournament's
  `enter_player`), so users get no feedback and rejected events stay on the
  tape. guardian2 already guards against another app's `judged` events —
  tapes are being shared, which makes event shapes a public interface.
- **Placement doesn't compose.** guardian2 and file_browser carry warnings
  ("no `server!()` mark, the caller hops"; "on_connect must NOT call a
  marked flow"), and every `sim_*` duplicates a real flow. A mark meaning
  "be on the server" (a no-op if already there) would delete all of it.

## Still open (pre-ship list, kept for honesty)

- Exact surface: `store.wait` vs a general `wait([...])` with
  `store.changed(...)` as one waitable among clicks, timers, and LLM streams.
- Cancellation semantics for spawned panels, and what a cancelled flow
  parked mid-hop does.
- Ordering when a story's own morph and a woken panel's morph race for the
  same element (tag renders with the commit point they read at; the browser
  drops older).
- The dev-mode lint: warn when a flow reads paths it never waits on. A
  warning, never an auto-listener.
- Whether reducers stay in hop (needed if browsers ever run them for
  optimistic UI) or move to Rust (types, derived event schemas).

## What shipping taught (2026-10-11)

- **Single path first.** `store.wait(path)` shipped; general `wait([...])`
  with clicks/timers/LLM waitables did not. agent.hop's approval gate is
  still the mouth hungry for it.
- **Cancel is handles + origins.** `spawn` is an expression (the child flow
  id); `cancel(handle)` kills one; `cancel_origin()` kills all my server
  flows (no arg = my session). Tab close kills by origin before the
  disconnect hook. A cancelled waiter simply never wakes.
- **read-seq rides everywhere.** Every store read stamps the flow's commit
  point; it rides hop packets AND replies, max-merged per flow. Miss the
  reply half and browser hops clobber the stamp to 0 — every wait wakes
  instantly in an infinite render chain (found live, fixed).
- **Panels are spawn chains, not loops.** `while true` with marks panics
  the CFG backend (missing fixup address), so the compiler rejects marks
  inside `while true` with a clear error. Bounded loops and for-loops
  with marks are fine. The panel shape: show, wait, cast-spawn the next
  iteration browser-side, return. The cast-spawn matters: a server-spawned
  child has Nil origin, so her `browser!()` dies — chain through your own
  session.
- **Coarse watches are honest.** Dashboards that read the whole store
  (guardian2, microblog) watch the root (`store.wait([])`). Precision, as
  designed, will live in reducer-kept indexes, not a smarter bridge.
- **Direct appends drain too.** The harness `append` resumes woken waiters
  inline — otherwise store-only tests would never observe a wake.
- **Replies have side effects on arrival order.** A woken panel's
  done-reply and chain cast land in the same server pass as the writer's
  reply — socket tests must accept any order.
