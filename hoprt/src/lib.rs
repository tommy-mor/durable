//! hoprt — the Hop runtime and the hopc compiler, all native.
//!
//! `value` is the closed value model (docs/hop-values.md); `ir` + `interp`
//! are the stack VM whose executions suspend at hops; `rt` is the flow
//! runtime (packets, LIFO reply stacks, casts); `compiler` is hopc,
//! lowering `.hop` placement marks into segment functions under stable hop
//! ids; `harness` runs the simulated cluster; `store` binds the durable
//! JSONL/RocksDB store to the server VM; `serve` is hopd, speaking CBOR
//! binary over WebSockets.

#[cfg(feature = "server")]
pub mod auth;
pub mod builtins;
pub mod compiler;
#[cfg(feature = "server")]
pub mod harness;
pub mod interp;
pub mod ir;
pub mod modules;
pub mod rt;
#[cfg(feature = "server")]
pub mod serve;
#[cfg(feature = "server")]
pub mod store;
pub mod value;
