# durable / hop

A full-stack app is one `.hop` file. You write a function body once, mark
the line where it should move to the server (`server!()`) or back to the
browser (`browser!()`), and the compiler splits it there. Each piece runs
on the right machine, and the compiler works out which variables cross
the network. State lives on an append-only event tape, which a
deterministic reducer folds into a RocksDB projection. Restart the server
and every app picks up where it left off.

```hop
fn add_todo(text) {
  server!();                                   // everything below runs on the server
  store.append({ type = "add", text = text }); // the only way state changes
  let todos = store.items(["todos"]);
  cast browsers {                              // fan out to every open tab
    hui.render("#app", todo_app(todos));
  }
}
```

The client never sends code or RPC names, only hop ids and data. Both sides
hold the same compiled program, and the server stamps identity from the
socket, so a tab can't claim to be someone else.

## Quick start

You need a Rust toolchain. `wasm-pack` is only needed for real browser tabs.

```bash
# Simulated cluster: two fake browsers and a server, with every packet printed
cargo run -p hoprt

# Real browsers: build the wasm interpreter once, then serve an app
wasm-pack build hop-web --target web
cargo run -p hoprt --bin hopd -- hoprt/hop/todo.hop
# open http://localhost:9000 in two tabs; they stay in sync.
# kill hopd and start it again; the board is still there.
```

The first build compiles RocksDB, so it takes a few minutes.

## What's in the box

| Crate | What it is |
|-------|------------|
| [`durable/`](durable/) | Event-sourced state runtime: JSONL log, typed reducers, nested RocksDB structures addressed by paths, and a serializable query language. Works as a standalone Rust library. |
| [`durable-derive/`](durable-derive/) | `#[derive(Durable)]` for typed schemas. |
| [`hoprt/`](hoprt/) | The Hop language: compiler (`hopc`), IR interpreter, flow runtime, simulated-cluster harness, and the `hopd` server (HTTP plus CBOR over WebSockets). |
| [`hop-web/`](hop-web/) | The same interpreter compiled to wasm, with DOM bindings. This is what runs in the tab. |

The layers stack: `hop-web` uses the platform-neutral core of `hoprt`
(built without the default `server` feature), and the server half of
`hoprt` uses `durable`.

## How it fits together

```text
   browser tab (wasm VM)                      hopd (server VM)
 ┌───────────────────────┐   CBOR packets   ┌───────────────────────────┐
 │ same compiled program │ ◄──────────────► │ same compiled program     │
 │ hui.render → DOM      │  call/cast/reply │ store.append(event)       │
 │ dom.get, handlers     │                  │   → log.jsonl (fsync)     │
 └───────────────────────┘                  │   → reduce(event)         │
                                            │   → RocksDB projection    │
                                            │ effects: bash, llm.*      │
                                            └───────────────────────────┘
```

**Placement marks.** `server!()` and `browser!()` split a function into
segments with stable ids (`add_todo:1`). Reaching a mark suspends the flow,
ships the live variables, and resumes on the other side. `cast` is
fire-and-forget, sent to `server`, `browsers`, `session(sid)`, or
`user(uid)`. An error in a remote segment unwinds back to the line that
made the hop.

**The store is one function.** `store(path)` is a query, and
`store([..., store.set(v)])` is a write. The last element of the path
decides which. Writes are only legal inside `fn reduce(event)`. Everywhere
else, state changes go through `store.append(event)`, which returns the
event's sequence number. Inside the reducer, reads see state as of the
previous event.

**One value model.** Values are nil, bool, int, float, string, bytes,
array, map, and tagged. They have the same CBOR encoding in memory, on the
wire, and on disk. Closures stay on the VM that created them and never
cross the network.

**Effects suspend the flow.** `bash(cmd)` and `llm.stream` / `llm.next`
run on the server, off-thread. The VM keeps serving other tabs while they
run. Results come back as data (`{ ok = false, stderr = ... }`), not
exceptions. The reducer rejects effects, so replay stays deterministic.

## Example apps

All of these live in [`hoprt/hop/`](hoprt/hop/). Run any of them with
`cargo run -p hoprt --bin hopd -- hoprt/hop/<name>.hop`.

- **`todo.hop`**: multiplayer todos, the smallest complete app.
- **`chat.hop`**: rooms of messages, broadcast to every tab.
- **`microblog.hop`**: follows and posts, with home timelines filled in when a post is written.
- **`ranking.hop`**: pairwise votes stored as a weighted graph. Decay is an event on the tape.
- **`tournament.hop`**: collaborative brackets in three formats. Reopening a match cascades the result change through later rounds.
- **`agent.hop`**: a coding agent in one file. It streams LLM replies, asks for approval before each `bash` command, and keeps per-user threads. The transcript is on the tape, so it survives restarts.
- **`guardian.hop`**: a play/pausable guardian agent over a sorted file index — judges pairs on its own scheduler heartbeat, streams every verdict live to all tabs.
- **`demo.hop`**: no UI. It exercises round trips, a remote error, chained hops, and nested casts in the simulated cluster.

`agent.hop` needs `OPENAI_API_KEY`. `OPENAI_BASE_URL` (any OpenAI-compatible
endpoint) and `HOP_LLM_MODEL` are optional. `hopd` reads a `.env` file in the
working directory, and real environment variables override it.

## Guardians (server-scheduled flows)

An app that declares `tick_every_ms` (int, milliseconds) and defines `fn
tick()` gets `tick` fired as a plain server flow on that interval — no
browser required. Guardians are the third flow kind: not browser-initiated,
not reducers, just ordinary server flows with full store and effect access
(`llm.*`, `bash`, `store.append`, casts). Play/pause lives in the app's own
store (`guardian.running` in the demo), so the loop is durable and
replayable; the ticker fires blindly and `tick` early-returns when paused.
A guardian's non-determinism stays at the edge exactly like a browser tab's:
the tape records the events it appends, never its reasoning loop.

## `hopd`

```text
hopd <app.hop> [http_port=9000] [ws_port=9001] [--data hop-data] [--web hop-web/pkg] [--log]
```

If the app declares `schema` and `fn reduce`, `hopd` opens a store in
`--data`: `log.jsonl` (the source of truth) and `proj/` (the RocksDB
projection, which can be rebuilt from the log). `--log` prints every packet
in readable form. `hopc <app.hop>` prints the compiled IR listing.

## Embedding

`hoprt` is a library. If an app needs natives beyond the built-in set
(`json`, `markdown`, `str`, `bash`, `llm`), register a module before calling
`serve`:

```rust
use hoprt::modules::{register, Module, NativeDef, NativeKind};
use hoprt::value::Value;

fn shout(args: Vec<Value>) -> Result<Value, String> {
    match args.first() {
        Some(Value::Str(s)) => Ok(Value::Str(s.to_uppercase().into())),
        _ => Err("shout: expected a string".into()),
    }
}

register(Module {
    name: "loud",
    natives: vec![NativeDef { name: "loud.shout", kind: NativeKind::Pure(shout) }],
});
// then hoprt::serve::serve(...)
```

Pure natives run on either side and are allowed in reducers. Effects run
only on the server. To add one, declare it here, add an executor in
`serve.rs`, and add a fake in the harness.

## Using `durable` on its own

You don't need Hop to use `durable`. It's a typed, event-sourced state
layer for Rust: reducers write through typed paths, and clients
(including Python) can append events and query, but can't mutate the
projection directly. See [`durable/README.md`](durable/README.md).

## Tests

```bash
cargo test --workspace
```

The tests check behavior from the outside: wire transcripts, the exact
variables shipped at each hop, rejection of forged identities over real
sockets, and `store.verify()`, which checks that the live projection
matches a fresh replay of the tape. They never assert on VM internals.
Every test scenario ends by checking that no flow is left suspended.

## Docs

The design notes live in [`durable/docs/`](durable/docs/):

- [`hop-values.md`](durable/docs/hop-values.md): the value model (normative).
- [`hop-store.md`](durable/docs/hop-store.md): the `store(path)` language (normative).
- [`hop-ir.md`](durable/docs/hop-ir.md): the IR, the interpreter, effects, and `match`.
- [`design.md`](durable/docs/design.md): durable's key layout, cost model, and runtime.
- [`hop-semantics.md`](durable/docs/hop-semantics.md) and [`network-lua.md`](durable/docs/network-lua.md): history from when Hop compiled to Lua. They're useful for the reasoning, but their file paths are out of date.
- [`ramalite.md`](durable/docs/ramalite.md): an exploration of pairing durable with a Clojure dialect.

## Limits

- One server process writes the store. There's no multi-process access and no distribution.
- Map iteration follows encoded-byte order, not logical key order.
- When a schema changes, drop the projection and replay the tape. Don't migrate in place.

## License

MIT OR Apache-2.0.
