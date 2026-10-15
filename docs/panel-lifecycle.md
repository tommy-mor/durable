# Flow lifecycle — keep the loop, kill the incantation

*Status: shared understanding, 2026-10-15. Decided via grill: the birth
form is called `flow`; one breaking commit; no backwards compatibility;
no URL work. Implemented same day. Two mechanics worth naming that the
grill left implicit: birth site is spawner site (a `flow` inside a
server segment starts server-side, inside a tab segment tab-side), and
a hop targeting the VM it runs on executes inline with no wire — so a
mark to the side you're already on is a no-op stay, and birth site
never changes what a panel does. `on_connect` runs on the server VM
owned by the tab (origin/user stamped, zero args): addressed by
ownership, not by execution site. This doc records what was decided
and why.*

## The problem

Every wait-style app pastes some version of this (`todo.hop`, `chat.hop`,
`microblog.hop`, `ranking.hop`, `guardian2.hop`):

```hop
// One panel per tab. Kill strays first (reconnects double-open), then
// start the panel.
fn open_board() {
  server!();
  cancel_origin();
  browser!();
  spawn todo_panel();
}

fn on_connect(sid) {
  cast session(sid) {
    open_board();
  }
}
```

To write it the author must know five runtime facts, none of which is
"todos": panels sleep server-side, reconnects double-open flows, `spawn`
from the server side won't land in the tab, `on_connect` fires
server-side, `cancel_origin` kills everything of yours. Copy-pasted
across five apps with apologies attached — the language missing a
concept.

The loop is not the problem and does not change:

```hop
fn chat_panel(room) {
  server!();
  while true {
    let snap = paint(room);
    browser!();
    hui.render("#app", chat_view(snap.room, snap.rooms, snap.messages));
    server!();
    store.wait(["rooms", room, "messages"]);
  }
}
```

## Decision: one birth form, called `flow`

`flow` is a **statement**, not a call — a special grammar form, deeply
baked with its own IR op (not desugared). A function call would evaluate
(and run) the callee in the caller; the form captures *what to run*
plus *with what*:

```hop
fn send(tid) {
  ...
  server!();
  store.append({ type = "user", tid = tid, text = text });
  paint_all();
  flow ("turn-" .. tid) run_turn(tid);
}
```

- The key expression and the args evaluate eagerly **in the caller**
  (`tid` is `send`'s param — no magic, same scope as today's `spawn`).
- The callee body never runs in the sender. The runtime takes the
  evaluated key + args, atomically kills any same-key flow of mine
  (both halves plus its store watch, via the existing `cancel`-packet
  release path), and starts the new flow with those args. No frame
  with zero or two holders — reconnect double-open becomes impossible
  by construction.
- Lifespan is determined by the body, not the keyword: a
  `while true + wait` body is a render vigil; a straight-line body is
  a one-shot. No separate "panel" vs "thread" spelling.
- Keys live per-session (tab), matching today's `cancel_origin`
  scope. Replacements are logged in `--trace` like any hop.

Desugar was considered and rejected: kill-then-spawn can't be atomic
and drags `spawn`'s eager-handle semantics along.

## Decision: cut `spawn`, `cancel`, `cancel_origin` outright

No deprecation, no dual support. Deleted: the natives, the `cancel`
packet path for app code, every app/test call site — one breaking
commit.

Inventory (verified 2026-10-15): panel births die in 5 places
(`chat:103`, `todo:104`, `microblog:160`, `ranking:105`,
`guardian2:342`) with their `cancel_origin` preambles (`chat:101`,
`todo:102`, `microblog:158`, `ranking:103`, `guardian2:340`).
Transient chains that need a new story:

- `agent.hop:221,240,260` — `run_turn` resumes become
  `flow ("turn-" .. tid) run_turn(tid)`. A direct call is wrong here:
  the turn suspends on `llm.stream` for minutes, and calling would
  nest that suspension inside the click flow and park the tab. The
  flow form keeps sender-finishes/turn-runs-alone, and per-tid
  replace gives the no-overlap property the `judging` flag currently
  enforces by hand.
- `agent.hop:466` — `fetch_models` becomes `flow "models"
  fetch_models()`. A direct call would hold first paint hostage on
  one HTTP round trip for a datalist nobody reads before typing.
  (One-shot vs vigil is a body shape, not a second keyword.)
- All `sim_*` / `demo_*` drivers (`todo`, `chat`, `microblog`,
  `ranking`, `demo`, `pipeline.rs go()`) leave app source; the
  harness drives named flows by key.
- Zero-spawn apps (`tournament`, `guardian`, `file_browser`) are
  already closest to the target shape and mostly just lose their
  `on_connect` wrappers.

Dataflow note: background-to-foreground communication is always the
tape. `fetch_models` resumes from `llm.models` → appends `models` →
JSONL → reducer → RocksDB → `paint_user` reads it → `cast user(uid)`
renders. No shared memory between flows exists or is added.

## Decision: `on_connect()` takes nothing, runs owned by the tab

```hop
fn on_connect() {
  open_board();
}
```

- No `sid` param, no `cast session(sid)` wrapper, no marks to reach
  the tab. Connecting *is* starting the flow. Execution stays on the
  server VM; ownership is what arrives addressed (origin/user stamped,
  zero args), and `flow` births inside it land under the tab.
- Identity is ambient, read via the existing `session()` / `user()`
  host calls (precedent: `tournament`'s `me = session()`, `chat`'s
  `sid = session()`). `agent`'s dead `sid` dies; its live `uid` logic
  is unchanged in behavior.
- Old `(sid)` / `(sid, uid)` signatures break: called with zero args,
  the params read `nil`.

## Decision: URLs stay dead

No `url()`, no `navigate()`, no `on_navigate`. The URL bar stays
frozen; tab state lives in flow args; docs stop listing routing as
open work. Tabs are cursors over the tape, not documents; the
shareable address of "what I'm seeing," if it ever exists, is a tape
seq, not a path string.

## After

```hop
fn open_board() {
  flow "board" todo_panel();
}

fn on_connect() {
  open_board();
}
```

```hop
fn open_room(room) {
  flow "chat" chat_panel(room);
}
```

No `cancel_origin`, no triple-mark dance, no `cast session(sid)`,
no `#current-tid`, no `views` map, no `cwd` tape events. Panel loops
don't change a character.

## Tradeoffs kept

- **No declarative flows, no auto-watch.** The loop stays a story the
  code tells out loud. The read-without-wait lint stays a warning,
  never an auto-listener.
- **Key collision is silent kill.** Two features sharing `"board"`
  replace each other without error. Mitigation: keys visible in
  `--explain`, replacements in `--trace`. Revisit namespacing if
  convention fails.
- **Back-button semantics unanswered.** Transient flow locals are
  lost on navigation like any reload. Same rule as today.
- **History not addressable.** URLs name *where*, never *when*.
  Deliberately out of scope.

## Migration: one breaking commit

Compiler (new `flow` form; delete `spawn`/`cancel`/`cancel_origin`;
zero-param `on_connect` only), runtime (keyed registry, atomic
replace, `session_connect` passes nothing), all ten apps, all tests,
`hop-wait.md` / `hop-store.md` / README touch-ups. Kill list above.
