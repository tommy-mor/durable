//! hoprt — hop runtime host and the hopc compiler.
//!
//! `harness` runs the simulated cluster (one server VM, n browser VMs,
//! packet queue). `compiler` is hopc v0: it turns `.hop` source (brace
//! syntax with placement marks) into the Lua that `lua/app.lua` previously
//! hand-wrote — segment functions registered under stable hop ids.

pub mod compiler;
pub mod harness;
pub mod serve;
pub mod store;
