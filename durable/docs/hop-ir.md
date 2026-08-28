# Hop IR and interpreter — architecture of the rewrite

Lua is removed. Hop owns its execution model: `hopc` compiles `.hop` to a
Hop IR; a Rust interpreter executes it with natively suspendable flows;
values are [hop-values.md](hop-values.md) values everywhere — memory,
wire, store — encoded as CBOR. The wire is binary; debugging is the
interpreter's log mode (`--log` in hopd, `verbose` in the harness), which
renders packets and values in diagnostic notation.

## Why an IR at all

The Lua lowering worked because segments were closures. The IR keeps that
shape — it does not need first-class continuations:

- A **program** is a set of functions. Some are named (`add_todo`), some
  are **segments** minted by mark-splitting (`add_todo:1`, `todo_view:l1:1`,
  `stroke:c2`) — same stable hop ids as before; the wire grammar is
  unchanged except for its encoding.
- A segment takes one argument (the vars map) and destructures it in its
  prologue. What ships is still hopc's static liveness set. All VMs load
  the identical program; the wire carries ids + data, never code.
- `at` is one instruction: the interpreter returns `Suspended` to the
  runtime with the flow's saved frames; the reply resumes them. No Lua
  coroutines — a flow is a plain Rust value (a stack of frames).

## Crate layout

```text
hoprt/
  src/value.rs      Value, eq/ord/truthiness, CBOR encode/decode, diagnostics
  src/ir.rs         Program, Function, Instr, constants
  src/interp.rs     stack VM: run/resume, Outcome{Done, Suspend, Error}
  src/rt.rs         flows, LIFO reply stacks, packets, register/at/cast
                    (port of hoprt.lua, ~1:1)
  src/builtins.rs   stdlib natives + native hui (render/handlers)
  src/compiler.rs   .hop lexer/parser (kept) + IR codegen (replaces Lua text)
  src/harness.rs    in-process simulated cluster over the native VM
  src/store.rs      store natives on the server VM (direct Value↔CBOR;
                    the unsafe Lua reducer wrapper is gone)
  src/serve.rs      hopd: HTTP + WebSocket (binary CBOR frames), server VM
```

`durable`, RocksDB, and the HTTP/WS stack sit behind the default `server`
feature; the core (value/ir/interp/rt/builtins/compiler) compiles for
wasm32. The `hop-web` crate is the browser backend: the same interpreter
built for wasm32 with a web-sys DOM platform (`BrowserVm`), replacing the
Lua-era wasmoon+glue. glue.js is a dumb pipe — it opens the WebSocket and
forwards binary frames both ways; all protocol knowledge (the hello, the
packet grammar, hui handler ids) lives in the wasm. hopd ships the app's
`.hop` *source* to the tab, which compiles it with the same compiler —
the wire carries hop ids and data, never code, so both sides must simply
hold the same program. Build with `wasm-pack build hop-web --target web`;
hopd serves the pkg from `--web` (default `hop-web/pkg`).

## The IR

Stack machine, one shared constant pool per program.

```text
Const(k)  Nil  True  False
LoadLocal(i)  StoreLocal(i)  LoadGlobal(k)  StoreGlobal(k)
MakeArray(n)  MakeMap(n)          // n values / n k-v pairs from the stack
GetIndex  SetIndex  GetField(k)  SetField(k)
BinOp(op)  UnOp(op)               // ops per hop-values.md
Jump(d)  JumpIfFalse(d)  Dup  Pop // and/or compile to short-circuit jumps
Call(nargs)                       // callee under args; natives included
Closure(fn, ncaps)                // captures by value from the stack
IterNew  IterNext(d)              // for-loops; arrays 0.., maps key-order
Return
At(hop_k)                         // pops target, vars → Suspend
Cast(hop_k)                       // pops target, vars → send, continue
Spawn                             // pops closure → new flow on this VM
Session                           // pushes session identity
```

Suspension: `At` returns control to the runtime with the frame stack;
the runtime parks it under (flow, VM) — LIFO, hops nest like calls — and
sends the call packet. A reply pushes the value onto the parked frame's
stack and resumes; an error unwinds the interpreter and propagates as an
error packet toward the flow origin (semantics identical to the Lua era,
asserted by the same transcript tests).

## Surface-language stdlib (the Lua names die with Lua)

Ported apps use a flat, small stdlib: `push(xs, v)`, `len(x)`,
`sort(xs, cmp?)`, `floor(n)`, `tostring(v)`, `tonumber(v)`, `print(...)`,
plus `session()` and the `store.*` / `hui.*` / `dom.*` modules. Arrays
are 0-based (hop-values.md); `for i, v in xs` runs 0..len-1; map
iteration is key-ordered and deterministic. The store is one callable —
`store(path)`, where collecting and terminal navigators in the path decide
query vs mutation — specified in [hop-store.md](hop-store.md).

## What stays true across the rewrite

- Hop ids and packet shapes (kind/flow/to/hop/vars/origin/reply_to) —
  now CBOR maps on a binary transport.
- Liveness: ship sets computed statically, asserted on the wire.
- Identity on connection: hopd stamps origin/reply_to from the socket.
- hui: closures in attributes stay VM-local; handler ids per render.
- The store contract: tape → reducer → projection, `verify()` = replay
  equality. Reducers are now IR functions run by the same interpreter —
  no cross-language bridge, one value model end to end.
- The conformance style: tests assert transcripts, wire vars, and store
  state — which is why they survive the backend swap.
