# hop — one program, one tape

hop is a language for networked applications where the whole app — the UI,
the state, the history, the agents — is a single program on a single event
tape. You write a function body once, mark where it should cross the
network, and the compiler works out what travels. There is no frontend
codebase and no backend codebase. There is one `.hop` file, and it is the
application.

This crate (`hoprt`) is the runtime: `hopc` the compiler, `hopd` the app
server, the simulated cluster for tests, and the module batteries.

## The smallest app

```hop
schema {
  todos: list<record {
    text: leaf,
    done: leaf
  }>
}

fn reduce(event) {
  match event {
    add { text } => {
      store(["todos", store.push({ text = text, done = false })]);
    }
    toggle { id } => {
      let t = store(["todos", id]);
      store(["todos", id, store.set({ text = t.text, done = !t.done })]);
    }
  }
}

fn add_todo(text) {
  server!();                                   // the next lines run on the server
  store.append({ type = "add", text = text }); // the only way state changes
  let todos = store.items(["todos"]);
  cast browsers {                              // fan out to every open tab
    hui.render("#app", todo_view(todos));
  }
}

fn todo_view(todos) {
  let rows = [];
  for i, t in todos {
    push(rows, [:li, {}, t.text]);
  }
  return [:ul, {}, rows];
}
```

An `hop` file declares **items**: `schema { … }` (the store's shape —
see below), `server let` (a global evaluated once on the server at boot
— config, constants) and `fn` (flows).

A **flow** is a function that may cross the network. Flows originate in the
browser (a button's `onclick`, `on_connect`, `spawn`) or in the server
scheduler (`tick`).

## Placement marks: the network is punctuation

`server!()` and `browser!()` are legal anywhere a flow body reaches —
top level, branches, loops, match arms. A mark is a **call edge**: the
current piece parks its whole frame (locals, iterators, hidden state —
nothing crosses implicitly), the far side runs as a hop function with a
stable id (`add_todo:1`), and its reply resumes the parked frame exactly
where it left off.

What crosses each hop is computed by backwards liveness over the
control-flow graph the compiler builds from your body:

- **Ship set** — the vars the far piece needs: `LiveIn(entry)`, packed as
  the hop's `vars` map.
- **Reply vars** — what comes home: `LiveIn(continuation) ∩ (defs of the
  piece ∪ what crossed)`. Shipping is call-by-copy, so a mutation on the
  far side (`push`, field/index writes) rides the reply back when the
  caller still needs the value.

Marks alternate sides (`server!()` while on the server is a compile
error). A hop inside a `cast` is rejected — a cast is a fire-and-forget
message whose body already runs on the target.

**Debugging the network** is two instruments:

- `hopc app.hop --explain` — the lamp: one line per hop id showing exactly
  what ships in and what replies back.
- `hopd --trace <file>` — the lantern: a greppable execution trace. Every
  assign (slot-resolved to its source name), every hop with its ship set,
  every tape event as JSON.

The compiler also **warns** when a locally-bound variable is live on the
far side but not in scope at the mark (defined on only some paths): it
would read as `nil` there.

Errors unwind across hops to the flow's origin — a failure in a server
piece surfaces at the click that started the flow.

## The store: schema, tape, projection

State is a durable, event-sourced store. You declare a **schema**, and the
only way state changes is `store.append(event)` inside a server flow; your
**reduce** function folds events into the projection.

```hop
schema {
  guardian: record {
    ticks: sum,        // a counter; missing reads as 0
    err: leaf          // a scalar; missing reads as nil
  },
  files: map<record { ... }>,   // a map of records
  feed: list<leaf>              // an append-only list
}

fn reduce(event) {
  match event {
    tick_mark {} => { store(["guardian", "ticks", store.add(1)]); }
    file { path, url, lines } => {
      store(["files", path, store.set({ url = url, lines = lines })]);
    }
  }
}
```

Reads are path-based: `store(["files", path])`, `store.items(["files"])`
(map entries as records with the key merged in as `id` — ready for
`for i, f in ...` rendering). Map keys are `any` by default; naming a
key kind (`map<string, …>`) validates reducer writes, as do `blob<…>`
shapes for opaque CBOR values (`map` inside, `record` outside — see
`durable/docs/hop-store.md`). Store maps iterate in *encoded key* order: a
key's bytes are length-prefixed in RocksDB, so shorter keys come first
(`"b"` before `"aa"`). This differs from the VM's value order for
in-memory maps; sort explicitly when display order matters.

**Presence is honest.** A missing leaf reads `nil`; a missing sum reads
`0`; a missing container reads `nil` (so ordinary truthiness *is*
presence). `store.has(path)` probes the raw keyspace when you need to
distinguish "absent" from "zero."

**A story can wait for the world to change.** `store.wait(path)` parks the
flow until a commit touches the watched subtree, then wakes her to show
again — no broadcasts, no refresh pokes. Watches are whole subtrees
(overlap checked both directions through `durable::watch`); dashboards
that read everything watch the root (`store.wait([])`). The commit point
of the flow's last read rides along invisibly, so a write landing between
the read and the wait wakes at once. See `docs/hop-wait.md`.

The tape is the source of truth (`log.jsonl`); the projection is RocksDB
(`proj/`), rebuildable from the tape at any time. Every event is appended
atomically with its reduce — a crash before commit simply replays.

Because everything is a fold, **projections are free**: billing is a sum
of token events, rankings are folds over votes, the invoice and the
history are the same data read differently.

## Views: hiccup and casts

Views are values. A node is `[:tag, { attrs }, children...]`; a plain
array is a fragment; strings and numbers are escaped text. A closure in an
attribute is an event handler — it runs as a flow, so it may hop.

```hop
[:button, { onclick = fn(e) { add_todo(dom.get("#new")); } }, "add"]
```

`cast browsers { hui.render("#app", view) }` sends the render to every
tab; `cast session(sid)` and `cast user(uid)` target narrower. `dom.get`
reads input values browser-side; their results ride into server flows as
ordinary arguments.

`markdown(text)` renders model output to a hiccup tree (text stays text;
hui escapes), the safe way to show LLM output.

## Effects: the module batteries

hop code runs on three kinds of hosts — the browser VM (wasm), the server
VM (hopd), and the reducer (replay). What a native may do is part of its
type:

- **Pure** natives run everywhere, including reducers: `json.encode /
  decode / first`, `markdown`, `str.has / before / cut / strip / replace`,
  `time.now`, and the core (`len`, `push`, `sort_by`, `type`, ...).
- **Effects** are server-side only and never legal in reducers. They
  suspend the flow; hopd executes them off-thread and the reply resumes:

```hop
let res = http.post(url, body, { Authorization = "Bearer " .. token });
let out = bash("ls -la");                       // bash -c — UNROOTED, see below
let text = fs.read("src/main.rs");              // rooted: --fs-root is the
let n = fs.count_lines("src/main.rs");          //   whole path policy
let files = git.files("*.rs");                  // tracked files (ls-files)
let files = git.files("*.rs", "subdir");
let cl = git.clone("https://github.com/x/y", "y");   // shallow clone
let h = llm.stream({ messages = [...], api_key = key });  // BYO key per call
let out = llm.next(h);    // out.delta streams; final has out.text/out.usage
```

`fs` and `git` resolve every path inside `hopd --fs-root`; absolute paths,
`..`, and symlink escapes are refused in the executor. `bash` is the
exception: it runs an arbitrary shell command with hopd's full permissions
and is not bound by `--fs-root`. Never build a `bash` command from data a
client or a model supplied; prefer `fs` / `git` whenever they can do the job. `llm` calls report
token usage (`out.usage.prompt_tokens` etc.) when the API provides it —
fold it onto the tape and billing is a projection.

Embedders add their own natives: register a `Module` with
`hoprt::modules::register` before the first VM (see the workspace README).

## The toolchain

```text
hopc <app.hop> [-o out.txt] [--explain]
hopd <app.hop> [http_port] [ws_port]
     [--data hop-data] [--web hop-web/pkg] [--fs-root <dir>]
     [--set key=value] [--trace <file>] [--log]
```

- **hopd** compiles the app, runs the server VM, and speaks CBOR over
  WebSockets to browser tabs (which boot the same program compiled to
  wasm). With a `schema` + `reduce` it opens the durable store at
  `--data`.
- **--set key=value** overrides a `server let` after compile — per-deploy
  config and secrets stay out of committed source.
- **--fs-root** roots the `fs`/`git` effects.
- **--trace** is the execution lantern (above).
- **--log** prints every packet.
- An app that declares `tick_every_ms` and defines `fn tick()` gets a
  server-scheduled heartbeat: the tick fires blindly and the app decides
  what to do — the play/pausable loop behind guardian agents.

## Language notes (the short version)

- Values: numbers, strings, bools, nil, arrays, maps (ordered), closures.
  Concatenation is `..`. Absence is `nil`.
- Control: `if/else`, `while`, `for k, v in x` (arrays index from 0; maps
  iterate key order), `match` on `.type` with field destructuring
  (`ok { value } => ...`), `return`.
- `spawn f(args)` starts a flow; it runs when the current piece finishes.
  `spawn` is an expression — its value is the child's handle (flow id).
  `cancel(handle)` kills one flow (stack and store watch); `cancel_origin()`
  kills all my server flows (room switches, tab close does this for you).
  `while true` bodies may not hold marks — write wait-loops as spawn
  chains: show, `store.wait`, spawn the next iteration, return.
- Assignment to an unbound name sets a process global (`x = 1` with no
  `let x` in scope is a global store — useful for per-process state like
  an ephemeral API key that must never touch the tape). The flip side:
  a typo'd local silently becomes a global.
- Lambdas are closures (`fn (e) { ... }`); captures are copied at
  creation. Lambdas may contain marks — this is what makes
  `onclick = fn(e) { server!(); ... }` work.

## Testing: the simulated cluster

`hoprt::harness::Cluster` runs an app on a simulated multi-tab cluster in
process: real fs/git effects (rooted at the cwd), a deterministic fake
LLM, and a full packet transcript (`host.log()`). Tests assert on wire
behavior — the exact vars shipped at each hop, what landed on the tape,
what rendered — never on VM internals. Every scenario ends by asserting
the cluster is quiescent: no flow left suspended.

## The normative docs

- `durable/docs/hop-values.md` — the value model.
- `durable/docs/hop-store.md` — the `store(path)` language and presence.
- `durable/docs/hop-ir.md` — the IR, the interpreter, effects, and `match`.

## Apps in this repo

- `hop/todo.hop` — the minimal shared-state app.
- `hop/demo.hop` — the pipeline demo: marks, casts, errors, liveness.
- `hop/guardian.hop` — a play/pausable LLM agent judging a codebase into
  slug-as-a-database, with token billing folded from the tape.
- `hop/agent.hop`, `hop/tournament.hop`, `hop/chat.hop`,
  `hop/microblog.hop`, `hop/ranking.hop` — older, smaller explorations.
