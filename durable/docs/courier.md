# The courier — three occupants of a Hop program

*Companion to [ramalite.md](./ramalite.md). That note derived this pipeline
from the storage layout, bottom-up:*

```text
append → reduce → commit {write keys} → ∩ {subscribed query ranges}
       → re-run only the hit queries → patch only those browsers
```

*This note arrives at the same pipeline from the opposite direction —
user stories, push versus pull — and finds the same shape. When an idea
is rediscovered from a second angle, it's usually real. Read this first
for the why, ramalite for the mechanism.*

## Three occupants, one job each

A running Hop app has three occupants, each with a different shape:

1. **The tab loop** lives in the browser and follows the user. It's
   imperative and story-shaped. Clicks come in, events go out. It never
   reads the store and never waits for a render — it has two inbound
   streams, the user's clicks and the courier's deliveries, and both are
   *pushed to it*. The loop flows around what arrives.
2. **The reducer** turns events into truth. It's pure, it's replayed, and
   it doesn't know anyone is watching. Same tape, same world, forever —
   which is why it stays in Hop (or any interpreted, effect-free
   language), never in Rust: nothing may smuggle a clock, a thread, or a
   HashMap iteration order into the one function every tape replays.
   Heavy deterministic meat (centrality, sorting, listing directories)
   lives behind native boundaries the reducer calls into, not in the
   reducer's language.
3. **The courier** turns truth into deliveries. It sees each commit's
   write set and pushes to whichever tabs are showing something that
   just changed, filtered by auth. It is the only occupant that knows
   who is watching what.

And that's the bystander's story given its own occupant. The actor's
story is the flow. The world's truth is the reducer. What everyone else
sees is the courier.

## The diff is already in hand

The "byte diffing the rocks folder" version is less of a joke than it
sounds. Each commit is literally a RocksDB write batch — a list of
exactly the keys that changed — and durable already holds it. The
courier doesn't discover what changed; it gets handed the diff. No
dependency tracking framework, no dirty flags: the write set and the
read set are byte prefixes, and prefix overlap is a comparison.
Over-subscription degrades to extra repaints, never missed ones — wrong
in the safe direction.

## Reads register themselves

The courier knows what each tab is showing without subscription code in
the app. When a tab's flow reads `server!(); store(["users"])`, the
runtime notes "this tab read `users/*`." Reads register interest,
writes trigger it, and the courier intersects them. App code mentions
neither.

Two consequences fall out:

- **Coalescing is correct by construction.** Views are pure functions
  of committed state, so intermediate renders were never observable
  truth — render latest, drop the rest. (The tape keeps every event;
  reads coalesce, writes never do.) Ephemeral traffic that *is* its
  intermediate values — keystrokes, cursor positions, LLM token streams
  — bypasses the store entirely on a volatile path, the way
  `paint_stream` already does. Durable state renders via courier;
  ephemeral progress via cast. That's the ontology, not a hack.
- **Auth lives in exactly one place.** The read permissions every
  design discussion circles back to finally get a home: the courier,
  right before anything leaves the server. Today's per-painter,
  per-handler checks collapse into one gate over (tab, path, commit).

## What the program looks like

```hop
schema {
  cwd: leaf = "",
  entries: leaf = [],
  selected: leaf,
}

fn tab_loop() {
  loop {
    let ev = dom.poll();          // browser-local, never ships
    match ev {
      click with target => {
        server!();
        store.append(route(target));  // wait for the write; renders arrive by courier
      }
      else => { }
    }
  }
}
```

`route` is pure input classification, `dom.poll` never crosses the
wire, the loop carries no state (anything it accumulated would ship on
every hop — the loop body must be amnesiac), and there is no `paint`
function anywhere in the file. The 60fps inputs never touch the tape;
only their committed outcomes become events.

## Build order

1. Prefix matching + read-registration behind a `subscribe` native,
   view by naming convention. Proves invalidation and lifecycle
   (subscribe/unsubscribe/disconnect) with no grammar work.
2. Syntax (`subscribe path as params => view`) once phase 1 shows
   where parameter-passing and destination predicates bite. The
   requirements doc is agent.hop's three painters: shared view,
   per-user view, per-thread stream.
3. The courier's auth gate, when reads register themselves — one gate,
   not N handler checks.

*Status: design note. Revisit with fresh eyes before building; the
load-bearing claims are (a) prefix overlap is sufficient invalidation,
(b) views-as-pure-functions permit latest-wins coalescing, and (c) the
reducer must stay interpreted for replay honesty.*
