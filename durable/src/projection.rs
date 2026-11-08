//! Projection identity: which reducer, over which shape, built the stored
//! projection.
//!
//! The tape is the truth; the projection is derived. Whether a derived
//! projection is still valid for the code opening it is a question durable
//! can answer itself, before any reducer runs, instead of every application
//! hand-rolling a version gate that runs after catch-up.
//!
//! The stamp is two `u64` leaves under the `__durable` namespace, next to the
//! applied offset:
//!
//! * `reducer` — the application's [`ProjectionId`], bumped by a human when a
//!   fold rule changes and history should be re-derived;
//! * `shape_hash` — [`shape_hash`] of the derived [`Shape`], which catches
//!   "added a leaf and forgot to bump" mechanically.
//!
//! # Crash analysis (see `docs/evolving-a-tape.md` for the operator view)
//!
//! * The stamp is written only by [`reset`], and only as the stamp of a store
//!   that has just been wiped. Wipe and stamp are two commits, both
//!   `SyncWal`: the batch layer resolves `DeletePrefix` AFTER point writes, so
//!   a stamp put in the same batch as the wipe would be deleted by it.
//! * Crash after the wipe, before the stamp: the store holds no `applied` and
//!   no stamp. The next open sees "fresh" (nothing applied, nothing stamped)
//!   and resets again. Nothing is lost; the projection was disposable.
//! * Crash mid catch-up: every apply commit moves `applied` and the data
//!   together, and the stamp was committed before the first apply. So
//!   `applied > 0` implies the stamp is the one the applying code asked for.
//! * Crash before the wipe: the old store is untouched; the next open
//!   decides again.
//! * Legacy store (no stamp, `applied > 0`, written by `Runtime::open` or by
//!   the pre-identity durable): under [`OnMismatch::Rebuild`] it is rebuilt
//!   once. It is never adopted, because no shape was ever recorded for it.
//!   Under [`OnMismatch::Refuse`] it is refused.
//! * Downgrade across this feature: a binary built before it ignores the
//!   stamp and advances `applied` under its own rules. Nothing in this crate
//!   can stop that; the rollback checklist in the docs deletes the projection
//!   instead. A newer binary opening a store stamped by a newer reducer
//!   simply rebuilds (projections are derived).

use std::fmt;
use std::fs::{File, OpenOptions as FsOpenOptions, TryLockError};
use std::path::Path;
use std::sync::Arc;

use sha2::{Digest as _, Sha256};

use crate::codec;
use crate::fault;
use crate::path::Path as DPath;
use crate::runtime::{Runtime, Tx, META_NS};
use crate::shape::field_segment;
use crate::{Db, Durability, Error, Leaf, Meta, Op, Result, Shape, Write};

/// Application-chosen semantics version of the projection's reducer.
///
/// Bump it when a fold rule changes and the history should be re-derived by
/// the new rule. Changing the reducer without bumping it is not detected
/// (no hash of code can tell which code changes matter); the shape hash only
/// catches changes to the projection's layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct ProjectionId {
    pub reducer: u64,
}

impl ProjectionId {
    pub const fn new(reducer: u64) -> Self {
        Self { reducer }
    }
}

/// What `open_with` does when the stored stamp disagrees with the wanted one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OnMismatch {
    /// Wipe the projection and replay the tape from zero. The default: a
    /// projection is derived, so this is always safe, only slower.
    #[default]
    Rebuild,
    /// Refuse to open, with [`Error::ProjectionMismatch`], touching nothing.
    Refuse,
}

/// The identity recorded in a projection: reducer id plus shape hash.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProjectionStamp {
    pub reducer: u64,
    pub shape_hash: u64,
}

impl ProjectionStamp {
    /// The stamp `open_with` would write for `reducer` over `schema`.
    pub fn new(reducer: u64, schema: &Shape) -> Self {
        Self {
            reducer,
            shape_hash: shape_hash(schema),
        }
    }
}

impl fmt::Display for ProjectionStamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "reducer={} shape={:016x}", self.reducer, self.shape_hash)
    }
}

/// Stable 64-bit hash of a [`Shape`], over a canonical byte encoding.
///
/// The encoding is versioned and defined here, never by `Hash` or
/// `DefaultHasher` (whose output may change across Rust releases or
/// platforms): a stamp written on one machine must match on another.
/// Encoding, all integers big-endian `u64`: a tag byte per node (`Leaf` 0,
/// `Sum` 1, `Map` 2, `List` 3, `Deque` 4, `Record` 5), then for collections
/// the child; for records the field count and each field as
/// (name length, name bytes, child). The hash is the first 8 bytes of
/// SHA-256 over `b"durable.shape.v1\0"` followed by that encoding.
///
/// Changing this encoding changes every hash and so forces one rebuild of
/// every projection. Do not change it casually.
pub fn shape_hash(shape: &Shape) -> u64 {
    let mut bytes = b"durable.shape.v1\0".to_vec();
    encode_shape(shape, &mut bytes);
    let digest = Sha256::digest(&bytes);
    u64::from_be_bytes(digest[..8].try_into().expect("8 bytes"))
}

fn put_u64(out: &mut Vec<u8>, n: u64) {
    out.extend_from_slice(&n.to_be_bytes());
}

fn encode_shape(shape: &Shape, out: &mut Vec<u8>) {
    match shape {
        Shape::Leaf => out.push(0),
        Shape::Sum => out.push(1),
        Shape::Map { of } => {
            out.push(2);
            encode_shape(of, out);
        }
        Shape::List { of } => {
            out.push(3);
            encode_shape(of, out);
        }
        Shape::Deque { of } => {
            out.push(4);
            encode_shape(of, out);
        }
        Shape::Record { fields } => {
            out.push(5);
            put_u64(out, fields.len() as u64);
            for (name, child) in fields {
                put_u64(out, name.len() as u64);
                out.extend_from_slice(name.as_bytes());
                encode_shape(child, out);
            }
        }
    }
}

/// Options for [`Runtime::open_with`](crate::Runtime::open_with).
///
/// `reducer` is shared (`Arc`) so the runtime can keep it for `verify` and
/// `rebuild`. Use [`OpenOptions::new`] for the common case.
pub struct OpenOptions<E> {
    pub schema: Shape,
    pub namespace: Option<String>,
    pub projection: ProjectionId,
    pub on_mismatch: OnMismatch,
    pub reducer: Reducer<E>,
}

/// A shared reducer: folds one event into the projection through a [`Tx`].
pub type Reducer<E> = Arc<dyn Fn(&mut Tx, &E) -> Result<()> + Send + Sync>;

impl<E> OpenOptions<E> {
    /// Options with no namespace and [`OnMismatch::Rebuild`].
    pub fn new(
        schema: Shape,
        projection: ProjectionId,
        reducer: impl Fn(&mut Tx, &E) -> Result<()> + Send + Sync + 'static,
    ) -> Self {
        Self {
            schema,
            namespace: None,
            projection,
            on_mismatch: OnMismatch::Rebuild,
            reducer: Arc::new(reducer),
        }
    }
}

fn reducer_leaf() -> DPath<Leaf<u64>> {
    stamp_leaf(1)
}

fn shape_leaf() -> DPath<Leaf<u64>> {
    stamp_leaf(2)
}

/// Drop the two identity stamp keys from a dump. Used where an identified
/// projection is compared with an unidentified replay of the same tape.
pub(crate) fn without_stamp(dump: Vec<(Vec<u8>, Vec<u8>)>) -> Vec<(Vec<u8>, Vec<u8>)> {
    let reducer = reducer_leaf().prefix().to_vec();
    let shape = shape_leaf().prefix().to_vec();
    dump.into_iter()
        .filter(|(k, _)| *k != reducer && *k != shape)
        .collect()
}

fn stamp_leaf(field: u32) -> DPath<Leaf<u64>> {
    let mut prefix = Vec::new();
    codec::put_segment(&mut prefix, META_NS.as_bytes());
    DPath::from_prefix(codec::child_key(&prefix, &field_segment(field)))
}

/// The stamp stored in `db`, if any. A half-written stamp is corruption:
/// both leaves are written in one commit and wiped together, so one without
/// the other cannot be produced by this crate.
pub(crate) fn read_stamp(db: &Db) -> Result<Option<ProjectionStamp>> {
    match (reducer_leaf().get(db)?, shape_leaf().get(db)?) {
        (None, None) => Ok(None),
        (Some(reducer), Some(shape_hash)) => Ok(Some(ProjectionStamp {
            reducer,
            shape_hash,
        })),
        _ => Err(Error::Corruption(
            "projection identity half-written (reducer or shape hash missing)".into(),
        )),
    }
}

pub(crate) fn stamp_writes_for(stamp: ProjectionStamp) -> [Write; 2] {
    [
        reducer_leaf().set(&stamp.reducer),
        shape_leaf().set(&stamp.shape_hash),
    ]
}

/// Wipe the whole projection, then stamp it, as two `SyncWal` commits in
/// that order. Afterwards `applied` is absent (zero) and the stamp is
/// `stamp` (or absent for an unidentified runtime).
pub(crate) fn reset(db: &Db, stamp: Option<ProjectionStamp>) -> Result<()> {
    fault::hit("projection.wipe")?;
    let mut tx = Tx::new(db, Meta { seq: 0, ts_ms: 0 });
    tx.write(Write::new(Op::DeletePrefix { prefix: Vec::new() }));
    tx.commit(Durability::SyncWal)?;
    if let Some(stamp) = stamp {
        fault::hit("projection.stamp")?;
        db.apply(&stamp_writes_for(stamp), Durability::SyncWal)?;
    }
    fault::hit("projection.stamped")?;
    Ok(())
}

/// Take the exclusive writer lock on the tape, creating the file if needed.
/// The lock is advisory (`flock`) and lives as long as the returned handle.
pub(crate) fn lock_tape(log_path: &Path) -> Result<File> {
    let file = FsOpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .open(log_path)
        .map_err(|e| Error::Log(e.to_string()))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => Err(Error::Log(format!(
            "tape {} is locked by another writer",
            log_path.display()
        ))),
        Err(TryLockError::Error(e)) => {
            Err(Error::Log(format!("lock tape {}: {e}", log_path.display())))
        }
    }
}

/// Take a shared lock, refusing if a live writer holds the exclusive one.
/// Used by offline readers (`rewrite`) so they never read a tape mid-seal.
pub(crate) fn lock_tape_shared(log_path: &Path) -> Result<File> {
    let file = File::open(log_path).map_err(|e| Error::Log(e.to_string()))?;
    match file.try_lock_shared() {
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => Err(Error::Log(format!(
            "tape {} is locked by a live writer; stop it before rewriting",
            log_path.display()
        ))),
        Err(TryLockError::Error(e)) => {
            Err(Error::Log(format!("lock tape {}: {e}", log_path.display())))
        }
    }
}

impl<E> Runtime<E> {
    /// The identity this runtime was opened with (or the stamp it found, for
    /// a legacy `open`). `None` for an unidentified legacy store.
    pub fn projection_stamp(&self) -> Option<ProjectionStamp> {
        self.identity
    }

    /// Every key/value pair of the projection, sorted by key. Includes the
    /// `__durable` bookkeeping keys (applied offset, identity stamp).
    pub fn dump(&self) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        crate::equiv::dump_db(&self.db)
    }

    /// A stable 32-byte digest of [`Runtime::dump`]. Equal projections give
    /// equal digests on every platform; see [`crate::equiv::digest_pairs`].
    pub fn digest(&self) -> Result<[u8; 32]> {
        Ok(crate::equiv::digest_pairs(&self.dump()?))
    }
}
