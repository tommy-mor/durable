//! # durable
//!
//! A pure-Rust event-sourced application-state runtime: a self-sealing JSONL
//! event tape, a typed reducer, and a rebuildable `redb` projection built
//! around **typed paths** and **ordered keys**.
//!
//! ```
//! use durable::{Db, Durability, Durable, Leaf, Map, Set, Sum};
//!
//! #[derive(Durable)]
//! struct Store {
//!     scores: Map<String, Sum<i64>>,
//!     tags: Set<String>,
//!     title: Leaf<String>,
//! }
//!
//! # fn main() -> durable::Result<()> {
//! let db = Db::open_memory()?;
//! let root = Store::root();
//!
//! db.txn(Durability::SyncWal, |tx| {
//!     root.scores().add(tx, "alice", 10)?;
//!     root.scores().add(tx, "alice", 5)?;
//!     root.tags().insert(tx, "rust")?;
//!     root.title().set(tx, &"leaderboard".to_string())?;
//!     Ok(())
//! })?;
//!
//! let r = db.read()?;
//! assert_eq!(root.scores().get(&r, "alice")?, 15);
//! assert_eq!(root.scores().len(&r)?, 1);
//! assert!(root.tags().contains(&r, "rust")?);
//! assert_eq!(root.title().get(&r)?, Some("leaderboard".to_string()));
//! # Ok(())
//! # }
//! ```

pub mod census;
pub mod codec;
mod equiv;
pub mod fault;
pub mod key;
mod path;
mod projection;
mod rewrite;
pub mod runtime;
mod schema;
pub mod store;
pub mod watch;

use serde::{de::DeserializeOwned, Serialize};
use thiserror::Error;

pub use std::cmp::Reverse;

pub use durable_derive::Durable;
pub use equiv::{digest_pairs, fold_dump, testing, Dump};
pub use key::{Key, KeyEncode, KeyLookup, KeyRange, OrderedF64, PrefixOf};
pub use path::{Dir, EntryIter, KeyIter, MapIter, Path, SeqIter, SumMapIter};
pub use projection::{
    schema_hash, OnMismatch, OpenOptions, ProjectionId, ProjectionStamp, Reducer,
};
pub use rewrite::{dry_run, rewrite, Emit, Mode, RewriteReport, Step};
pub use runtime::{AppendOutcome, JsonlLog, Meta, Record, Runtime, TapeFile};
#[cfg(feature = "fault-injection")]
pub use runtime::{FailMode, FailingFile, FakeDisk};
pub use schema::{Deque, Leaf, List, Map, Schema, Set, Sum, Summable};
pub use store::{Db, Read, ReadTx, Tx};
pub use watch::{prefix_overlap, watch_hit, WatchRange};

/// Errors returned by durable operations.
#[derive(Error, Debug)]
pub enum Error {
    /// `redb` storage engine failure.
    #[error("store error: {0}")]
    Store(String),
    #[error("serialization error: {0}")]
    Serialize(String),
    #[error("deserialization error: {0}")]
    Deserialize(String),
    #[error("data corruption: {0}")]
    Corruption(String),
    #[error("event log error: {0}")]
    Log(String),
    #[error("reducer error: {0}")]
    Reducer(String),
    /// Insufficient free disk space below the configured floor (tape untouched).
    #[error("insufficient disk space: {available} bytes available, {required} required")]
    NoSpace { available: u64, required: u64 },
    /// An idempotency key was reused with a different event payload.
    #[error("idempotency conflict for key {id:?}: different payload already committed")]
    Conflict { id: String },
    /// Replay hit a reducer error on an already-committed tape event; further
    /// appends are refused until reopened with a fixed reducer.
    #[error("runtime halted at seq {seq}: {reason}")]
    Halted { seq: u64, reason: String },
    /// Stored projection identity disagrees with the requested one under
    /// [`OnMismatch::Refuse`]. Nothing was written.
    #[error("projection mismatch: stored {stored:?}, wanted {wanted}")]
    ProjectionMismatch {
        stored: Option<ProjectionStamp>,
        wanted: ProjectionStamp,
    },
}

/// Result alias used throughout the crate.
pub type Result<T> = std::result::Result<T, Error>;

/// Durability policy for standalone [`Db`] write transactions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Durability {
    /// Flush and fsync on commit (survives power loss).
    SyncWal,
    /// Equivalent to [`Durability::SyncWal`] on `redb` (`Durability::Immediate`).
    WalOnly,
    /// Skip fsync on commit (`redb::Durability::None`). Use for projections
    /// rebuildable from a durable tape.
    DisableWal,
}

/// CBOR-encode a leaf value.
pub(crate) fn encode_value<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    ciborium::ser::into_writer(value, &mut bytes).map_err(|e| Error::Serialize(e.to_string()))?;
    Ok(bytes)
}

/// CBOR-decode a stored leaf value.
pub(crate) fn decode_value<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    ciborium::de::from_reader(bytes).map_err(|e| Error::Deserialize(e.to_string()))
}
