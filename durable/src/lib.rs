//! # durable
//!
//! Event-sourced application state: a typed reducer writes a RocksDB
//! projection; a serializable query language reads it. Clients append events
//! and issue queries. They do not mutate indexes.
//!
//! The projection itself is still deeply nested, precisely updatable
//! RocksDB-backed data, built around **paths as data**.
//!
//! Instead of serializing a big struct into one blob, you describe your data with
//! a *schema* of composable types — [`Leaf`], [`Map`], [`List`], [`Deque`],
//! [`Sum`], and your own `#[derive(Durable)]` structs — and address any location
//! with a typed [`Path`]. A path lowers to a deterministic RocksDB key with no
//! I/O, so a mutation touches exactly the keys it names and nothing else.
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

use std::path::Path as FsPath;
use std::sync::Arc;

use rocksdb::{Options, WriteBatch, WriteOptions, DB as RocksDb};
use serde::{de::DeserializeOwned, Serialize};
use thiserror::Error;

pub use durable_derive::Durable;
pub use dynpath::{cbor_to_json, json_to_cbor, navs_for, Location};
pub use path::Path;
pub use query::{
    entries, explain, one, project, select, subtree, CostClass, Expr, Nav, Plan, Predicate, Query,
    Terminal,
};
pub use runtime::{JsonlLog, Meta, Record, Runtime, Tx};
pub use schema::{Deque, Leaf, List, Map, Schema, Sum, Summable};
pub use shape::{Describe, Shape};

/// Errors returned by durable operations.
#[derive(Error, Debug)]
pub enum Error {
    #[error("rocksdb error: {0}")]
    RocksDb(#[from] rocksdb::Error),
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

    /// Consume the write, yielding its operation.
    pub fn into_op(self) -> Op {
        self.op
    }
}

/// A handle to an open durable database.
///
/// Cheap to clone (an `Arc` around the RocksDB handle). Durable assumes a single
/// writer process; serialize writes at the application layer.
#[derive(Clone)]
pub struct Db {
    inner: Arc<RocksDb>,
}

impl Db {
    /// Open (or create) a durable database at `path`.
    pub fn open<P: AsRef<FsPath>>(path: P) -> Result<Self> {
        let mut opts = Options::default();
        opts.create_if_missing(true);
        opts.set_merge_operator_associative("durable.sum", schema::sum_merge);
        let db = RocksDb::open(&opts, path)?;
        Ok(Self {
            inner: Arc::new(db),
        })
    }

    pub(crate) fn raw(&self) -> &RocksDb {
        &self.inner
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
/// Blind writes are recorded immediately. Stateful appends ([`Batch::push_back`]
/// / [`Batch::push`]) are resolved at commit time against the current collection
/// length, so several appends in one batch land at contiguous indices and the
/// whole batch commits as one RocksDB write (one WAL flush).
pub struct Batch {
    db: Db,
    inner: WriteBatch,
    prefix_deletes: Vec<Vec<u8>>,
    appends: Vec<PendingAppend>,
}

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
            inner: WriteBatch::default(),
            prefix_deletes: Vec::new(),
            appends: Vec::new(),
        }
    }

    /// Record a reified write in this batch.
    pub fn write(&mut self, write: Write) {
        match write.into_op() {
            Op::Put { key, value } => self.inner.put(key, value),
            Op::Delete { key } => self.inner.delete(key),
            Op::Merge { key, value } => self.inner.merge(key, value),
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
        self.inner.put(key, value);
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
    pub fn commit_with(mut self, durability: Durability) -> Result<()> {
        self.resolve_prefix_deletes()?;
        self.resolve_appends()?;

        match durability {
            Durability::SyncWal => {
                self.db.raw().write(self.inner)?;
                self.db.raw().flush_wal(true)?;
            }
            Durability::WalOnly => {
                self.db.raw().write(self.inner)?;
            }
            Durability::DisableWal => {
                let mut opts = WriteOptions::default();
                opts.disable_wal(true);
                self.db.raw().write_opt(self.inner, &opts)?;
            }
        }
        Ok(())
    }

    fn resolve_prefix_deletes(&mut self) -> Result<()> {
        for prefix in std::mem::take(&mut self.prefix_deletes) {
            match codec::prefix_upper_bound(&prefix) {
                Some(end) => self.inner.delete_range(&prefix, &end),
                None => {
                    // Range extends to the end of the keyspace: scan and delete.
                    let iter = self.db.raw().iterator(rocksdb::IteratorMode::From(
                        &prefix,
                        rocksdb::Direction::Forward,
                    ));
                    for item in iter {
                        let (key, _) = item?;
                        if !key.starts_with(&prefix) {
                            break;
                        }
                        self.inner.delete(&key);
                    }
                }
            }
        }
        Ok(())
    }

    fn resolve_appends(&mut self) -> Result<()> {
        use std::collections::HashMap;
        // Group appends by (collection, end) so contiguous appends get contiguous
        // indices and each counter is read exactly once.
        let mut order: Vec<(Vec<u8>, u8)> = Vec::new();
        let mut grouped: HashMap<(Vec<u8>, u8), Vec<Vec<u8>>> = HashMap::new();
        for append in std::mem::take(&mut self.appends) {
            let tag = match append.end {
                AppendEnd::ListBack => 0u8,
                AppendEnd::DequeBack => 1u8,
                AppendEnd::DequeFront => 2u8,
            };
            let group_key = (append.coll_prefix, tag);
            let entry = grouped.entry(group_key.clone()).or_insert_with(|| {
                order.push(group_key);
                Vec::new()
            });
            entry.push(append.value);
        }

        for (coll_prefix, tag) in order {
            let values = grouped.remove(&(coll_prefix.clone(), tag)).unwrap();
            match tag {
                0 => self.resolve_list_back(&coll_prefix, values)?,
                1 => self.resolve_deque_end(&coll_prefix, values, true)?,
                2 => self.resolve_deque_end(&coll_prefix, values, false)?,
                _ => unreachable!(),
            }
        }
        Ok(())
    }

    fn resolve_list_back(&mut self, coll_prefix: &[u8], values: Vec<Vec<u8>>) -> Result<()> {
        let len_key = codec::meta_key(coll_prefix, b"len");
        let mut len = read_u64(&self.db, &len_key)?.unwrap_or(0);
        for value in values {
            let elem = codec::child_key(coll_prefix, &codec::order_u64(len));
            self.inner.put(&elem, &value);
            len += 1;
        }
        self.inner.put(&len_key, len.to_le_bytes());
        Ok(())
    }

    fn resolve_deque_end(
        &mut self,
        coll_prefix: &[u8],
        values: Vec<Vec<u8>>,
        back: bool,
    ) -> Result<()> {
        let head_key = codec::meta_key(coll_prefix, b"head");
        let tail_key = codec::meta_key(coll_prefix, b"tail");
        let mut head = read_i64(&self.db, &head_key)?.unwrap_or(0);
        let mut tail = read_i64(&self.db, &tail_key)?.unwrap_or(0);
        for value in values {
            if back {
                let elem = codec::child_key(coll_prefix, &codec::order_i64(tail));
                self.inner.put(&elem, &value);
                tail += 1;
            } else {
                head -= 1;
                let elem = codec::child_key(coll_prefix, &codec::order_i64(head));
                self.inner.put(&elem, &value);
            }
        }
        self.inner.put(&head_key, head.to_le_bytes());
        self.inner.put(&tail_key, tail.to_le_bytes());
        Ok(())
    }
}

pub(crate) fn read_u64(db: &Db, key: &[u8]) -> Result<Option<u64>> {
    match db.raw().get(key)? {
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
    match db.raw().get(key)? {
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
