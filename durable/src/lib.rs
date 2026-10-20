//! # durable
//!
//! Event-sourced application state: a typed reducer writes a storage-backend
//! projection; a serializable query language reads it. Clients append events
//! and issue queries. They do not mutate indexes.
//!
//! The projection itself is still deeply nested, precisely updatable
//! disk-backed data, built around **paths as data**.
//!
//! Instead of serializing a big struct into one blob, you describe your data with
//! a *schema* of composable types — [`Leaf`], [`Map`], [`List`], [`Deque`],
//! [`Sum`], and your own `#[derive(Durable)]` structs — and address any location
//! with a typed [`Path`]. A path lowers to a deterministic storage key with no
//! I/O, so a mutation touches exactly the keys it names and nothing else.
//!
//! The backend is a compile-time choice (`backend-rocksdb` by default,
//! `backend-redb` / `backend-sled` for pure-Rust engines with no C++
//! toolchain); see [`store`] for the contract each engine implements.
//!
//! Terminal operations on a path return reified [`Write`] values (not side
//! effects). Compose several into one atomic [`Batch`] and commit them with an
//! explicit [`Durability`] policy.
//!
//! ```
//! use durable::{Db, Durable, Durability, Leaf, Map, Sum};
//!
//! #[derive(Durable)]
//! struct Store {
//!     scores: Map<String, Sum<i64>>,
//!     title: Leaf<String>,
//! }
//!
//! // `#[derive(Durable)]` also generates a `StoreFields` navigator trait,
//! // in scope wherever `Store` is.
//!
//! # fn main() -> durable::Result<()> {
//! let dir = tempfile::tempdir().unwrap();
//! let db = Db::open(dir.path())?;
//!
//! let root = Store::root();
//! let alice = "alice".to_string();
//! db.apply(
//!     &[
//!         root.scores().key(&alice).add(10), // blind merge, no read
//!         root.scores().key(&alice).add(5),
//!         root.title().set(&"leaderboard".to_string()),
//!     ],
//!     Durability::SyncWal,
//! )?;
//!
//! assert_eq!(root.scores().key(&alice).get(&db)?, 15);
//! assert_eq!(root.title().get(&db)?, Some("leaderboard".to_string()));
//! # Ok(())
//! # }
//! ```

mod codec;
pub mod dynpath;
mod path;
pub mod query;
pub mod runtime;
mod schema;
mod shape;
pub mod store;
pub mod watch;

#[cfg(not(any(
    feature = "backend-rocksdb",
    feature = "backend-redb",
    feature = "backend-sled"
)))]
compile_error!(
    "durable: enable exactly one storage backend feature \
     (`backend-rocksdb`, `backend-redb`, or `backend-sled`)"
);
#[cfg(any(
    all(feature = "backend-rocksdb", feature = "backend-redb"),
    all(feature = "backend-rocksdb", feature = "backend-sled"),
    all(feature = "backend-redb", feature = "backend-sled"),
))]
compile_error!("durable: only one storage backend feature may be enabled at a time");

use std::path::Path as FsPath;
use std::sync::Arc;

use serde::{de::DeserializeOwned, Serialize};
use store::{FinalOp, Store};
use thiserror::Error;

pub use durable_derive::Durable;
pub use dynpath::{cbor_to_json, json_to_cbor, navs_for, Location};
pub use path::Path;
pub use query::{
    entries, explain, one, project, select, subtree, CostClass, Expr, Nav, Plan, Predicate, Query,
    Terminal,
};
pub use runtime::{JsonlLog, Meta, Record, Runtime, TapeFile, Tx};
#[cfg(feature = "fault-injection")]
pub use runtime::{FailMode, FailingFile, FakeDisk};
pub use schema::{Deque, Leaf, List, Map, Schema, Sum, Summable};
pub use shape::{Describe, Shape};
pub use watch::{prefix_overlap, watch_hit, watch_prefix, write_range, WatchRange};

/// Errors returned by durable operations.
#[derive(Error, Debug)]
pub enum Error {
    #[cfg(feature = "backend-rocksdb")]
    #[error("rocksdb error: {0}")]
    RocksDb(#[from] rocksdb::Error),
    /// Storage backend failure (redb/sled; surrealkv when it lands).
    #[error("store error: {0}")]
    Store(String),
    #[error("serialization error: {0}")]
    Serialize(String),
    #[error("deserialization error: {0}")]
    Deserialize(String),
    #[error("data corruption: {0}")]
    Corruption(String),
    #[error("query error: {0}")]
    Query(String),
    #[error("event log error: {0}")]
    Log(String),
    #[error("reducer error: {0}")]
    Reducer(String),
}

/// Result alias used throughout the crate.
pub type Result<T> = std::result::Result<T, Error>;

/// CBOR-encode a value for leaf storage or key encoding.
pub(crate) fn encode_value<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    ciborium::ser::into_writer(value, &mut bytes).map_err(|e| Error::Serialize(e.to_string()))?;
    Ok(bytes)
}

/// CBOR-decode a stored value.
pub(crate) fn decode_value<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    ciborium::de::from_reader(bytes).map_err(|e| Error::Deserialize(e.to_string()))
}

/// Durability policy for a committed [`Batch`] or [`Db::apply`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Durability {
    /// Write through the WAL and fsync it before returning (survives power loss).
    SyncWal,
    /// Write through the WAL without forcing an fsync.
    WalOnly,
    /// Skip the WAL entirely. Use only for projections rebuildable from another
    /// durable source of truth.
    DisableWal,
}

/// A single reified storage operation.
///
/// `Op` is the type-erased lowering of a typed terminal operation. It is plain
/// data: you can build, inspect, log, and store a list of ops, then apply them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    /// Blind put of a value at an exact key.
    Put { key: Vec<u8>, value: Vec<u8> },
    /// Blind delete of an exact key.
    Delete { key: Vec<u8> },
    /// Delete every key under `prefix` (a whole subtree, including its root leaf).
    DeletePrefix { prefix: Vec<u8> },
    /// Blind associative merge (used by [`Sum`]).
    Merge { key: Vec<u8>, value: Vec<u8> },
    /// Append to a list; resolved against the current length at commit.
    ListPush { prefix: Vec<u8>, value: Vec<u8> },
    /// Append to a deque back; resolved at commit.
    DequePushBack { prefix: Vec<u8>, value: Vec<u8> },
    /// Push to a deque front; resolved at commit.
    DequePushFront { prefix: Vec<u8>, value: Vec<u8> },
}

/// A typed, reified mutation produced by a terminal path operation.
///
/// A `Write` wraps a single [`Op`]. Collect several and hand them to
/// [`Db::apply`] (or push them onto a [`Batch`]) to commit atomically.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Write {
    op: Op,
}

impl Write {
    pub(crate) fn new(op: Op) -> Self {
        Self { op }
    }

    /// The underlying reified operation.
    pub fn op(&self) -> &Op {
        &self.op
    }

    /// The key ranges this write touches: exact keys for point ops,
    /// whole-subtree prefixes for collection pushes and prefix deletes.
    /// The courier's waking set — what a commit reports about itself.
    pub fn ranges(&self) -> Vec<WatchRange> {
        vec![watch::write_range(self)]
    }

    /// Consume the write, yielding its operation.
    pub fn into_op(self) -> Op {
        self.op
    }
}

/// A handle to an open durable database.
///
/// Cheap to clone (an `Arc` around the storage handle). Durable assumes a single
/// writer process; serialize writes at the application layer.
#[derive(Clone)]
pub struct Db {
    inner: Arc<Store>,
}

impl Db {
    /// Open (or create) a durable database at `path`.
    ///
    /// The on-disk format is backend-specific (see [`store`]); switching
    /// backends means replaying the event tape into a fresh directory.
    pub fn open<P: AsRef<FsPath>>(path: P) -> Result<Self> {
        let store = Store::open(path.as_ref())?;
        Ok(Self {
            inner: Arc::new(store),
        })
    }

    /// Point read against committed state.
    pub(crate) fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.inner.get(key)
    }

    /// Forward scan starting at the first key `>= from`.
    pub(crate) fn iter_from(&self, from: &[u8]) -> Result<store::StoreIter<'_>> {
        self.inner.iter_from(from)
    }

    /// Forward scan of the whole keyspace.
    pub(crate) fn iter_start(&self) -> Result<store::StoreIter<'_>> {
        self.inner.iter_start()
    }

    /// Start an atomic batch of writes.
    pub fn batch(&self) -> Batch {
        Batch::new(self.clone())
    }

    /// Apply reified writes atomically with the given durability policy.
    pub fn apply(&self, writes: &[Write], durability: Durability) -> Result<()> {
        let mut batch = self.batch();
        for write in writes {
            batch.write(write.clone());
        }
        batch.commit_with(durability)
    }

    /// Apply a single write with the given durability policy.
    pub fn run(&self, write: Write, durability: Durability) -> Result<()> {
        self.apply(std::slice::from_ref(&write), durability)
    }
}

/// An atomic batch of writes plus deferred collection appends.
///
/// Blind writes are recorded immediately, in order. Stateful appends
/// ([`Batch::push_back`] / [`Batch::push`]) and prefix deletes are resolved
/// at commit time against committed state, then the whole op stream commits
/// as one atomic backend write (one WAL flush on RocksDB, one transaction
/// on redb).
pub struct Batch {
    db: Db,
    point: Vec<PointOp>,
    prefix_deletes: Vec<Vec<u8>>,
    appends: Vec<PendingAppend>,
}

/// One blind point op, in arrival order.
pub(crate) enum PointOp {
    Put { key: Vec<u8>, value: Vec<u8> },
    Delete { key: Vec<u8> },
    Merge { key: Vec<u8>, value: Vec<u8> },
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum AppendEnd {
    /// Append to the tail of a [`List`]; counter is `len`, index is `len`.
    ListBack,
    /// Append to the back of a [`Deque`]; counter is `tail`, index is `tail`.
    DequeBack,
    /// Push to the front of a [`Deque`]; counter is `head`, index is `head - 1`.
    DequeFront,
}

pub(crate) struct PendingAppend {
    pub(crate) coll_prefix: Vec<u8>,
    pub(crate) end: AppendEnd,
    pub(crate) value: Vec<u8>,
}

impl Batch {
    fn new(db: Db) -> Self {
        Self {
            db,
            point: Vec::new(),
            prefix_deletes: Vec::new(),
            appends: Vec::new(),
        }
    }

    /// Record a reified write in this batch.
    pub fn write(&mut self, write: Write) {
        match write.into_op() {
            Op::Put { key, value } => self.point.push(PointOp::Put { key, value }),
            Op::Delete { key } => self.point.push(PointOp::Delete { key }),
            Op::Merge { key, value } => self.point.push(PointOp::Merge { key, value }),
            Op::DeletePrefix { prefix } => self.prefix_deletes.push(prefix),
            Op::ListPush { prefix, value } => self.appends.push(PendingAppend {
                coll_prefix: prefix,
                end: AppendEnd::ListBack,
                value,
            }),
            Op::DequePushBack { prefix, value } => self.appends.push(PendingAppend {
                coll_prefix: prefix,
                end: AppendEnd::DequeBack,
                value,
            }),
            Op::DequePushFront { prefix, value } => self.appends.push(PendingAppend {
                coll_prefix: prefix,
                end: AppendEnd::DequeFront,
                value,
            }),
        }
    }

    /// The database this batch will commit against. Reducers use this for
    /// committed-state reads; in-batch writes are not visible until commit.
    pub fn db(&self) -> &Db {
        &self.db
    }

    /// Record several reified writes.
    pub fn extend<I: IntoIterator<Item = Write>>(&mut self, writes: I) {
        for write in writes {
            self.write(write);
        }
    }

    pub(crate) fn raw_put(&mut self, key: Vec<u8>, value: Vec<u8>) {
        self.point.push(PointOp::Put { key, value });
    }

    /// Append `value` to the back of a leaf list (resolved at commit).
    pub fn push<T: Serialize>(&mut self, list: &Path<List<Leaf<T>>>, value: &T) -> Result<()> {
        self.appends.push(PendingAppend {
            coll_prefix: list.prefix().to_vec(),
            end: AppendEnd::ListBack,
            value: encode_value(value)?,
        });
        Ok(())
    }

    /// Append `value` to the back of a leaf deque (resolved at commit).
    pub fn push_back<T: Serialize>(
        &mut self,
        deque: &Path<Deque<Leaf<T>>>,
        value: &T,
    ) -> Result<()> {
        self.appends.push(PendingAppend {
            coll_prefix: deque.prefix().to_vec(),
            end: AppendEnd::DequeBack,
            value: encode_value(value)?,
        });
        Ok(())
    }

    /// Push `value` to the front of a leaf deque (resolved at commit).
    pub fn push_front<T: Serialize>(
        &mut self,
        deque: &Path<Deque<Leaf<T>>>,
        value: &T,
    ) -> Result<()> {
        self.appends.push(PendingAppend {
            coll_prefix: deque.prefix().to_vec(),
            end: AppendEnd::DequeFront,
            value: encode_value(value)?,
        });
        Ok(())
    }

    /// Commit the batch, fsyncing the WAL (equivalent to
    /// `commit_with(Durability::SyncWal)`).
    pub fn commit(self) -> Result<()> {
        self.commit_with(Durability::SyncWal)
    }

    /// Commit the batch with an explicit durability policy.
    ///
    /// Resolution is three-phase, in this order: blind point writes (in
    /// arrival order), prefix-delete expansion, resolved collection
    /// appends. In particular a [`Op::DeletePrefix`] in the batch removes
    /// keys put in the same batch no matter where it sits — every backend
    /// implements this same order.
    pub fn commit_with(mut self, durability: Durability) -> Result<()> {
        // Resolution order matches the old single-WriteBatch layout: point
        // ops first (arrival order), then prefix-delete expansion, then
        // resolved appends.
        let mut ops: Vec<FinalOp> = self
            .point
            .drain(..)
            .map(|op| match op {
                PointOp::Put { key, value } => FinalOp::Put(key, value),
                PointOp::Delete { key } => FinalOp::Delete(key),
                PointOp::Merge { key, value } => FinalOp::Merge(key, value),
            })
            .collect();
        ops.extend(self.resolve_prefix_deletes());
        ops.extend(
            self.resolve_appends()?
                .into_iter()
                .map(|(key, value)| FinalOp::Put(key, value)),
        );
        self.db.inner.write(ops, durability)
    }

    fn resolve_prefix_deletes(&mut self) -> Vec<FinalOp> {
        std::mem::take(&mut self.prefix_deletes)
            .into_iter()
            .map(|prefix| {
                let end = codec::prefix_upper_bound(&prefix);
                FinalOp::DeleteRange { start: prefix, end }
            })
            .collect()
    }

    fn resolve_appends(&mut self) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        use std::collections::HashMap;
        // Group appends by collection so contiguous appends get contiguous
        // indices and each counter is read exactly once. Deque appends keep
        // their end marker: back and front pushes to the SAME deque must
        // resolve against one shared head/tail read, otherwise the second
        // group would clobber the first's counter update with a stale value.
        enum Group {
            List(Vec<Vec<u8>>),
            Deque(Vec<(AppendEnd, Vec<u8>)>),
        }
        let mut order: Vec<Vec<u8>> = Vec::new();
        let mut grouped: HashMap<Vec<u8>, Group> = HashMap::new();
        for append in std::mem::take(&mut self.appends) {
            let entry = grouped.entry(append.coll_prefix.clone()).or_insert_with(|| {
                order.push(append.coll_prefix.clone());
                match append.end {
                    AppendEnd::ListBack => Group::List(Vec::new()),
                    AppendEnd::DequeBack | AppendEnd::DequeFront => {
                        Group::Deque(Vec::new())
                    }
                }
            });
            match (entry, append.end) {
                (Group::List(values), AppendEnd::ListBack) => values.push(append.value),
                (Group::Deque(items), end) => items.push((end, append.value)),
                _ => unreachable!("list and deque appends never share a prefix"),
            }
        }

        let mut out = Vec::new();
        for coll_prefix in order {
            match grouped.remove(&coll_prefix).unwrap() {
                Group::List(values) => self.resolve_list_back(&coll_prefix, values, &mut out)?,
                Group::Deque(items) => self.resolve_deque(&coll_prefix, items, &mut out)?,
            }
        }
        Ok(out)
    }

    fn resolve_list_back(
        &mut self,
        coll_prefix: &[u8],
        values: Vec<Vec<u8>>,
        out: &mut Vec<(Vec<u8>, Vec<u8>)>,
    ) -> Result<()> {
        let len_key = codec::meta_key(coll_prefix, b"len");
        let mut len = read_u64(&self.db, &len_key)?.unwrap_or(0);
        for value in values {
            let elem = codec::child_key(coll_prefix, &codec::order_u64(len));
            out.push((elem, value));
            len += 1;
        }
        out.push((len_key, len.to_le_bytes().to_vec()));
        Ok(())
    }

    /// Resolve mixed back/front appends to one deque against a single
    /// head/tail read, preserving arrival order.
    fn resolve_deque(
        &mut self,
        coll_prefix: &[u8],
        items: Vec<(AppendEnd, Vec<u8>)>,
        out: &mut Vec<(Vec<u8>, Vec<u8>)>,
    ) -> Result<()> {
        let head_key = codec::meta_key(coll_prefix, b"head");
        let tail_key = codec::meta_key(coll_prefix, b"tail");
        let mut head = read_i64(&self.db, &head_key)?.unwrap_or(0);
        let mut tail = read_i64(&self.db, &tail_key)?.unwrap_or(0);
        for (end, value) in items {
            match end {
                AppendEnd::ListBack => unreachable!("list appends never join a deque group"),
                AppendEnd::DequeBack => {
                    let elem = codec::child_key(coll_prefix, &codec::order_i64(tail));
                    out.push((elem, value));
                    tail += 1;
                }
                AppendEnd::DequeFront => {
                    head -= 1;
                    let elem = codec::child_key(coll_prefix, &codec::order_i64(head));
                    out.push((elem, value));
                }
            }
        }
        out.push((head_key, head.to_le_bytes().to_vec()));
        out.push((tail_key, tail.to_le_bytes().to_vec()));
        Ok(())
    }
}

pub(crate) fn read_u64(db: &Db, key: &[u8]) -> Result<Option<u64>> {
    match db.get(key)? {
        Some(bytes) => {
            if bytes.len() != 8 {
                return Err(Error::Corruption("expected 8-byte u64 meta".into()));
            }
            let mut buf = [0u8; 8];
            buf.copy_from_slice(&bytes);
            Ok(Some(u64::from_le_bytes(buf)))
        }
        None => Ok(None),
    }
}

pub(crate) fn read_i64(db: &Db, key: &[u8]) -> Result<Option<i64>> {
    match db.get(key)? {
        Some(bytes) => {
            if bytes.len() != 8 {
                return Err(Error::Corruption("expected 8-byte i64 meta".into()));
            }
            let mut buf = [0u8; 8];
            buf.copy_from_slice(&bytes);
            Ok(Some(i64::from_le_bytes(buf)))
        }
        None => Ok(None),
    }
}
