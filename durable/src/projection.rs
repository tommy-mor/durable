//! Projection identity: which reducer, over which Rust [`Schema`], built the
//! stored `redb` projection.
//!
//! The tape is the truth; the projection is derived. On [`Runtime::open_with`],
//! the stored stamp `{reducer, schema_hash}` in the `__durable` table is
//! compared against the requested one **before any reducer call**. On mismatch:
//! - [`OnMismatch::Rebuild`] (default) wipes all tables and replays from 0.
//! - [`OnMismatch::Refuse`] returns [`Error::ProjectionMismatch`] untouched.

use std::fmt;
use std::fs::{File, OpenOptions as FsOpenOptions, TryLockError};
use std::path::Path;
use std::sync::Arc;

use crate::fault;
use crate::runtime::Runtime;
use crate::store::{Db, Read, Tx, BOOKKEEPING_TABLE};
use crate::{Durability, Error, Result, Schema};

pub(crate) const KEY_STAMP_REDUCER: &[u8] = b"stamp_reducer";
pub(crate) const KEY_STAMP_SCHEMA: &[u8] = b"stamp_schema";

/// Application-chosen semantics version of the projection's reducer.
///
/// Bump it when a fold rule changes (or when a `Leaf<T>` struct's internal
/// fields change incompatibly) so history is re-derived on open. Structural
/// changes to the store (`#[derive(Durable)]` fields, `Map` key types, `Sum`
/// numeric types, `Leaf` type names, or `durable`'s format version) change
/// [`Schema::fingerprint`] automatically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct ProjectionId {
    pub reducer: u64,
}

impl ProjectionId {
    pub const fn new(reducer: u64) -> Self {
        Self { reducer }
    }
}

/// What [`Runtime::open_with`] does when the stored stamp disagrees with the
/// wanted one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OnMismatch {
    /// Wipe the projection and replay the tape from zero (default).
    #[default]
    Rebuild,
    /// Refuse to open with [`Error::ProjectionMismatch`], touching nothing.
    Refuse,
}

/// The identity recorded in a projection: reducer id plus schema fingerprint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProjectionStamp {
    pub reducer: u64,
    pub schema_hash: u64,
}

impl ProjectionStamp {
    /// The stamp `open_with` writes for `reducer` over schema `S`.
    pub fn for_schema<S: Schema>(reducer: u64) -> Self {
        Self {
            reducer,
            schema_hash: S::fingerprint(),
        }
    }

    /// Construct from an explicit `schema_hash` (e.g. for tests).
    pub const fn from_parts(reducer: u64, schema_hash: u64) -> Self {
        Self {
            reducer,
            schema_hash,
        }
    }
}

impl fmt::Display for ProjectionStamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "reducer={} schema={:016x}",
            self.reducer, self.schema_hash
        )
    }
}

/// Canonical 64-bit fingerprint of schema `S`.
pub fn schema_hash<S: Schema>() -> u64 {
    S::fingerprint()
}

/// A shared reducer: folds one event into the projection through a [`Tx`].
pub type Reducer<E> = Arc<dyn Fn(&mut Tx, &E) -> Result<()> + Send + Sync>;

/// Options for [`Runtime::open_with`].
pub struct OpenOptions<E> {
    pub schema_hash: u64,
    pub projection: ProjectionId,
    pub on_mismatch: OnMismatch,
    /// Optional minimum free bytes required on the tape's filesystem before
    /// appending (checked via `statvfs` on Unix). `None` disables the check.
    pub min_free_bytes: Option<u64>,
    pub reducer: Reducer<E>,
}

impl<E> OpenOptions<E> {
    /// Options for schema `S` with [`OnMismatch::Rebuild`] and no free-space floor.
    pub fn new<S: Schema>(
        projection: ProjectionId,
        reducer: impl Fn(&mut Tx, &E) -> Result<()> + Send + Sync + 'static,
    ) -> Self {
        Self {
            schema_hash: S::fingerprint(),
            projection,
            on_mismatch: OnMismatch::Rebuild,
            min_free_bytes: None,
            reducer: Arc::new(reducer),
        }
    }

    /// Set the mismatch policy.
    pub fn on_mismatch(mut self, mode: OnMismatch) -> Self {
        self.on_mismatch = mode;
        self
    }

    /// Require at least `bytes` of free disk space on the tape's filesystem
    /// before writing a new batch (returns [`Error::NoSpace`] below the floor).
    pub fn min_free_bytes(mut self, bytes: u64) -> Self {
        self.min_free_bytes = Some(bytes);
        self
    }
}

fn read_meta_u64(r: &impl Read, key: &[u8]) -> Result<Option<u64>> {
    match r.raw_get(BOOKKEEPING_TABLE, key)? {
        Some(bytes) => {
            if bytes.len() != 8 {
                return Err(Error::Corruption("expected 8-byte u64 metadata".into()));
            }
            let mut buf = [0u8; 8];
            buf.copy_from_slice(&bytes);
            Ok(Some(u64::from_le_bytes(buf)))
        }
        None => Ok(None),
    }
}

/// The stamp stored in `db`, if any. A half-written stamp is corruption.
pub(crate) fn read_stamp(r: &impl Read) -> Result<Option<ProjectionStamp>> {
    match (
        read_meta_u64(r, KEY_STAMP_REDUCER)?,
        read_meta_u64(r, KEY_STAMP_SCHEMA)?,
    ) {
        (None, None) => Ok(None),
        (Some(reducer), Some(schema_hash)) => Ok(Some(ProjectionStamp {
            reducer,
            schema_hash,
        })),
        _ => Err(Error::Corruption(
            "projection identity half-written (reducer or schema hash missing)".into(),
        )),
    }
}

pub(crate) fn write_stamp(tx: &mut Tx, stamp: ProjectionStamp) -> Result<()> {
    tx.raw_put(
        BOOKKEEPING_TABLE,
        KEY_STAMP_REDUCER,
        &stamp.reducer.to_le_bytes(),
    )?;
    tx.raw_put(
        BOOKKEEPING_TABLE,
        KEY_STAMP_SCHEMA,
        &stamp.schema_hash.to_le_bytes(),
    )?;
    Ok(())
}

/// Drop the two identity stamp keys from a dump.
pub(crate) fn without_stamp(
    dump: Vec<(String, Vec<u8>, Vec<u8>)>,
) -> Vec<(String, Vec<u8>, Vec<u8>)> {
    dump.into_iter()
        .filter(|(t, k, _)| {
            !(t == BOOKKEEPING_TABLE
                && (k.as_slice() == KEY_STAMP_REDUCER || k.as_slice() == KEY_STAMP_SCHEMA))
        })
        .collect()
}

/// Wipe the whole projection (O(tables) via `delete_table`) and optionally
/// stamp it, committed atomically with `SyncWal`.
pub(crate) fn reset(db: &Db, stamp: Option<ProjectionStamp>) -> Result<()> {
    fault::hit("projection.wipe")?;
    let mut tx = db.write_with(Durability::SyncWal)?;
    tx.raw_wipe_everything()?;
    if let Some(stamp) = stamp {
        fault::hit("projection.stamp")?;
        write_stamp(&mut tx, stamp)?;
    }
    tx.commit()?;
    fault::hit("projection.stamped")?;
    Ok(())
}

/// Take the exclusive writer lock on the tape, creating the file if needed.
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
    /// `open`). `None` for an unidentified store.
    pub fn projection_stamp(&self) -> Option<ProjectionStamp> {
        self.identity
    }

    /// Snapshot-consistent sorted dump of every table: `(table, key, value)`.
    pub fn dump(&self) -> Result<Vec<(String, Vec<u8>, Vec<u8>)>> {
        self.db.dump()
    }

    /// Stable 32-byte SHA-256 digest of [`Runtime::dump`].
    pub fn digest(&self) -> Result<[u8; 32]> {
        Ok(crate::equiv::digest_pairs(&self.dump()?))
    }
}
