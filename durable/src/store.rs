//! Pluggable key-value storage backends.
//!
//! Exactly one `backend-*` feature must be enabled (see `Cargo.toml`); the
//! choice is made by the depending crate. [`Db`](super::Db) talks to storage
//! only through [`Store`], so adding a backend means adding an enum variant
//! plus its `open` / `get` / scan / commit arms below — no call-site changes.
//!
//! Backend contract notes:
//!
//! * Single keyspace, lexicographically ordered, forward scans from a key.
//! * Commits are atomic: every [`FinalOp`] in one [`Store::write`] call lands
//!   together or not at all.
//! * [`Durability`] maps onto each engine's fsync story; see the commit arms.
//! * Merge ops fold through the shared [`crate::schema::fold_merge`]:
//!   natively at compaction time on RocksDB, read-modify-write inside the
//!   commit transaction on redb, and against a commit-time overlay on sled
//!   (whose batches have no merge op). All three preserve single-writer
//!   semantics exactly (and durable already assumes a single writer).
//! * On-disk formats differ per backend. Switching backends for an existing
//!   deployment means replaying the event tape into a fresh store directory —
//!   which is the supported migration path everywhere durable is used.

use std::path::Path as FsPath;

use super::{schema, Durability, Error, Result};

/// One committed point operation, in arrival order as recorded by [`Batch`](super::Batch).
///
/// Prefix deletes and collection appends are resolved to these primitives
/// before commit, so every backend sees the same flat op stream.
pub(crate) enum FinalOp {
    Put(Vec<u8>, Vec<u8>),
    Delete(Vec<u8>),
    Merge(Vec<u8>, Vec<u8>),
    /// Delete `[start, end)`; `end == None` means "to the end of the keyspace".
    DeleteRange {
        start: Vec<u8>,
        end: Option<Vec<u8>>,
    },
}

/// Forward key-order scan yielding owned pairs.
///
/// RocksDB iterates a live snapshot handle; redb materializes under one
/// read transaction (also snapshot-consistent). sled has no read
/// transactions, so scans materialize immediately — still exact under the
/// single-writer contract.
pub(crate) enum StoreIter<'a> {
    #[cfg(feature = "backend-rocksdb")]
    Rocksdb(rocksdb::DBIterator<'a>),
    #[cfg(feature = "backend-redb")]
    Redb(
        std::vec::IntoIter<(Vec<u8>, Vec<u8>)>,
        std::marker::PhantomData<&'a ()>,
    ),
    #[cfg(feature = "backend-sled")]
    Sled(
        std::vec::IntoIter<(Vec<u8>, Vec<u8>)>,
        std::marker::PhantomData<&'a ()>,
    ),
}

impl Iterator for StoreIter<'_> {
    type Item = Result<(Vec<u8>, Vec<u8>)>;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            #[cfg(feature = "backend-rocksdb")]
            StoreIter::Rocksdb(iter) => iter.next().map(|item| {
                item.map(|(k, v)| (k.into_vec(), v.into_vec()))
                    .map_err(Error::RocksDb)
            }),
            #[cfg(feature = "backend-redb")]
            StoreIter::Redb(iter, _) => iter.next().map(Ok),
            #[cfg(feature = "backend-sled")]
            StoreIter::Sled(iter, _) => iter.next().map(Ok),
        }
    }
}

pub(crate) enum Store {
    #[cfg(feature = "backend-rocksdb")]
    Rocksdb(rocksdb::DB),
    #[cfg(feature = "backend-redb")]
    Redb(redb::Database),
    #[cfg(feature = "backend-sled")]
    Sled(sled::Db),
}

impl Store {
    /// Open (or create) the store inside directory `path`.
    pub(crate) fn open(path: &FsPath) -> Result<Self> {
        #[cfg(feature = "backend-rocksdb")]
        {
            let mut opts = rocksdb::Options::default();
            opts.create_if_missing(true);
            opts.set_merge_operator_associative("durable.sum", schema::sum_merge);
            return rocksdb::DB::open(&opts, path)
                .map(Store::Rocksdb)
                .map_err(Error::RocksDb);
        }
        #[cfg(feature = "backend-redb")]
        {
            std::fs::create_dir_all(path).map_err(|e| Error::Log(e.to_string()))?;
            let db = redb::Database::create(path.join("store.redb")).map_err(store_err)?;
            // Reads fail on a missing table, so ensure it exists up front.
            let txn = db.begin_write().map_err(store_err)?;
            txn.open_table(redb_be::TABLE).map_err(store_err)?;
            txn.commit().map_err(store_err)?;
            return Ok(Store::Redb(db));
        }
        #[cfg(feature = "backend-sled")]
        {
            let db = sled::open(path).map_err(store_err)?;
            return Ok(Store::Sled(db));
        }
        #[allow(unreachable_code)]
        Err(Error::Log("no durable storage backend enabled".into()))
    }

    /// Point read.
    pub(crate) fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        match self {
            #[cfg(feature = "backend-rocksdb")]
            Store::Rocksdb(db) => db.get(key).map_err(Error::RocksDb),
            #[cfg(feature = "backend-redb")]
            Store::Redb(db) => redb_be::get(db, key),
            #[cfg(feature = "backend-sled")]
            Store::Sled(db) => sled_be::get(db, key),
        }
    }

    /// Forward scan starting at the first key `>= from`.
    pub(crate) fn iter_from(&self, from: &[u8]) -> Result<StoreIter<'_>> {
        match self {
            #[cfg(feature = "backend-rocksdb")]
            Store::Rocksdb(db) => Ok(StoreIter::Rocksdb(db.iterator(
                rocksdb::IteratorMode::From(from, rocksdb::Direction::Forward),
            ))),
            #[cfg(feature = "backend-redb")]
            Store::Redb(db) => Ok(StoreIter::Redb(
                redb_be::scan(db, Some(from))?.into_iter(),
                std::marker::PhantomData,
            )),
            #[cfg(feature = "backend-sled")]
            Store::Sled(db) => Ok(StoreIter::Sled(
                sled_be::scan(db, Some(from))?.into_iter(),
                std::marker::PhantomData,
            )),
        }
    }

    /// Forward scan of the whole keyspace.
    pub(crate) fn iter_start(&self) -> Result<StoreIter<'_>> {
        match self {
            #[cfg(feature = "backend-rocksdb")]
            Store::Rocksdb(db) => Ok(StoreIter::Rocksdb(
                db.iterator(rocksdb::IteratorMode::Start),
            )),
            #[cfg(feature = "backend-redb")]
            Store::Redb(db) => Ok(StoreIter::Redb(
                redb_be::scan(db, None)?.into_iter(),
                std::marker::PhantomData,
            )),
            #[cfg(feature = "backend-sled")]
            Store::Sled(db) => Ok(StoreIter::Sled(
                sled_be::scan(db, None)?.into_iter(),
                std::marker::PhantomData,
            )),
        }
    }

    /// Atomically commit a resolved op stream.
    pub(crate) fn write(&self, ops: Vec<FinalOp>, durability: Durability) -> Result<()> {
        match self {
            #[cfg(feature = "backend-rocksdb")]
            Store::Rocksdb(db) => {
                let mut batch = rocksdb::WriteBatch::default();
                for op in ops {
                    match op {
                        FinalOp::Put(k, v) => batch.put(k, v),
                        FinalOp::Delete(k) => batch.delete(k),
                        FinalOp::Merge(k, v) => batch.merge(k, v),
                        FinalOp::DeleteRange { start, end } => match end {
                            Some(end) => batch.delete_range(start, end),
                            None => {
                                // Range runs to the end of the keyspace: scan and
                                // delete point-wise (same fallback as before).
                                let iter = db.iterator(rocksdb::IteratorMode::From(
                                    &start,
                                    rocksdb::Direction::Forward,
                                ));
                                for item in iter {
                                    let (key, _) = item.map_err(Error::RocksDb)?;
                                    if !key.starts_with(&start) {
                                        break;
                                    }
                                    batch.delete(key);
                                }
                            }
                        },
                    }
                }
                match durability {
                    Durability::SyncWal => {
                        db.write(batch).map_err(Error::RocksDb)?;
                        db.flush_wal(true).map_err(Error::RocksDb)?;
                    }
                    Durability::WalOnly => {
                        db.write(batch).map_err(Error::RocksDb)?;
                    }
                    Durability::DisableWal => {
                        let mut opts = rocksdb::WriteOptions::default();
                        opts.disable_wal(true);
                        db.write_opt(batch, &opts).map_err(Error::RocksDb)?;
                    }
                }
                Ok(())
            }
            #[cfg(feature = "backend-redb")]
            Store::Redb(db) => redb_be::commit(db, ops, durability),
            #[cfg(feature = "backend-sled")]
            Store::Sled(db) => sled_be::commit(db, ops, durability),
        }
    }
}

/// Map any displayable backend error into [`Error::Store`].
fn store_err<E: std::fmt::Display>(e: E) -> Error {
    Error::Store(e.to_string())
}

/// Pure-Rust backend: redb (ACID B-tree, no C++ toolchain).
#[cfg(feature = "backend-redb")]
mod redb_be {
    use redb::{ReadableDatabase as _, ReadableTable as _};

    use super::*;

    pub(super) const TABLE: redb::TableDefinition<&[u8], &[u8]> =
        redb::TableDefinition::new("durable");

    pub(super) fn get(db: &redb::Database, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let txn = db.begin_read().map_err(store_err)?;
        let table = txn.open_table(TABLE).map_err(store_err)?;
        Ok(table
            .get(key)
            .map_err(store_err)?
            .map(|guard| guard.value().to_vec()))
    }

    /// Snapshot-consistent scan: materialized under one read transaction.
    /// `from == None` scans the whole keyspace.
    pub(super) fn scan(
        db: &redb::Database,
        from: Option<&[u8]>,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let txn = db.begin_read().map_err(store_err)?;
        let table = txn.open_table(TABLE).map_err(store_err)?;
        let mut out = Vec::new();
        match from {
            Some(start) => {
                let range = table.range(start..).map_err(store_err)?;
                for item in range {
                    let (k, v) = item.map_err(store_err)?;
                    out.push((k.value().to_vec(), v.value().to_vec()));
                }
            }
            None => {
                let range = table.range::<&[u8]>(..).map_err(store_err)?;
                for item in range {
                    let (k, v) = item.map_err(store_err)?;
                    out.push((k.value().to_vec(), v.value().to_vec()));
                }
            }
        }
        Ok(out)
    }

    pub(super) fn commit(
        db: &redb::Database,
        ops: Vec<FinalOp>,
        durability: Durability,
    ) -> Result<()> {
        let mut txn = db.begin_write().map_err(store_err)?;
        // redb offers no WAL-without-fsync level, so WalOnly upgrades to
        // Immediate (strictly stronger: process-crash safe, like SyncWal).
        txn.set_durability(match durability {
            Durability::SyncWal | Durability::WalOnly => redb::Durability::Immediate,
            // No durability promises: projections rebuildable from tape.
            Durability::DisableWal => redb::Durability::None,
        })
        .map_err(store_err)?;
        {
            let mut table = txn.open_table(TABLE).map_err(store_err)?;
            for op in ops {
                match op {
                    FinalOp::Put(k, v) => {
                        table
                            .insert(k.as_slice(), v.as_slice())
                            .map_err(store_err)?;
                    }
                    FinalOp::Delete(k) => {
                        table.remove(k.as_slice()).map_err(store_err)?;
                    }
                    FinalOp::Merge(k, operand) => {
                        // No native merge operator: fold read-modify-write
                        // inside the commit transaction. Exact for a single
                        // writer (durable's contract).
                        let current = table
                            .get(k.as_slice())
                            .map_err(store_err)?
                            .map(|guard| guard.value().to_vec());
                        match schema::fold_merge(current.as_deref(), &[operand.as_slice()]) {
                            Some(next) => {
                                table
                                    .insert(k.as_slice(), next.as_slice())
                                    .map_err(store_err)?;
                            }
                            None => {
                                table.remove(k.as_slice()).map_err(store_err)?;
                            }
                        }
                    }
                    FinalOp::DeleteRange { start, end } => {
                        // Collect first: the range borrows the table.
                        let mut keys = Vec::new();
                        match end {
                            Some(end) => {
                                let range = table
                                    .range(start.as_slice()..end.as_slice())
                                    .map_err(store_err)?;
                                for item in range {
                                    let (k, _) = item.map_err(store_err)?;
                                    keys.push(k.value().to_vec());
                                }
                            }
                            None => {
                                let range = table.range(start.as_slice()..).map_err(store_err)?;
                                for item in range {
                                    let (k, _) = item.map_err(store_err)?;
                                    let key = k.value().to_vec();
                                    if !key.starts_with(&start) {
                                        break;
                                    }
                                    keys.push(key);
                                }
                            }
                        }
                        for key in keys {
                            table.remove(key.as_slice()).map_err(store_err)?;
                        }
                    }
                }
            }
        }
        txn.commit().map_err(store_err)?;
        Ok(())
    }
}

/// Pure-Rust backend: sled (ordered B-tree, no C++).
///
/// sled's [`sled::Batch`] has no merge op, so [`FinalOp::Merge`] lowers to a
/// put/delete here against a commit-time overlay that replicates WriteBatch
/// order semantics exactly (prior puts/deletes/ranges in the same batch are
/// visible to later merges, and ranges cover earlier puts). The fold itself
/// is the shared [`schema::fold_merge`], so accumulator bytes are identical
/// on every backend.
#[cfg(feature = "backend-sled")]
mod sled_be {
    use std::collections::HashMap;

    use super::*;

    pub(super) fn get(db: &sled::Db, key: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(db.get(key).map_err(store_err)?.map(|v| v.to_vec()))
    }

    /// Eagerly materialized scan. `from == None` scans the whole keyspace.
    pub(super) fn scan(db: &sled::Db, from: Option<&[u8]>) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let mut out = Vec::new();
        match from {
            Some(start) => {
                for item in db.range(start..) {
                    let (k, v) = item.map_err(store_err)?;
                    out.push((k.to_vec(), v.to_vec()));
                }
            }
            None => {
                for item in db.iter() {
                    let (k, v) = item.map_err(store_err)?;
                    out.push((k.to_vec(), v.to_vec()));
                }
            }
        }
        Ok(out)
    }

    pub(super) fn commit(db: &sled::Db, ops: Vec<FinalOp>, durability: Durability) -> Result<()> {
        let mut batch = sled::Batch::default();
        // Committed-state overlay: Some(value) = put this batch,
        // None = deleted this batch. Lets later ops in the same commit
        // observe earlier ones, exactly like a RocksDB WriteBatch.
        let mut overlay: HashMap<Vec<u8>, Option<Vec<u8>>> = HashMap::new();
        for op in ops {
            match op {
                FinalOp::Put(k, v) => {
                    batch.insert(k.clone(), v.clone());
                    overlay.insert(k, Some(v));
                }
                FinalOp::Delete(k) => {
                    batch.remove(k.clone());
                    overlay.insert(k, None);
                }
                FinalOp::Merge(k, operand) => {
                    let base = match overlay.get(&k) {
                        Some(staged) => staged.clone(),
                        None => get(db, &k)?,
                    };
                    match schema::fold_merge(base.as_deref(), &[operand.as_slice()]) {
                        Some(next) => {
                            batch.insert(k.clone(), next.clone());
                            overlay.insert(k, Some(next));
                        }
                        None => {
                            batch.remove(k.clone());
                            overlay.insert(k, None);
                        }
                    }
                }
                FinalOp::DeleteRange { start, end } => {
                    let in_range = |k: &[u8]| {
                        k >= start.as_slice() && end.as_ref().is_none_or(|e| k < e.as_slice())
                    };
                    // Staged keys in range, plus the committed scan minus
                    // anything the overlay already covers.
                    let mut condemned: Vec<Vec<u8>> =
                        overlay.keys().filter(|k| in_range(k)).cloned().collect();
                    match &end {
                        Some(end) => {
                            for item in db.range(start.as_slice()..end.as_slice()) {
                                let (k, _) = item.map_err(store_err)?;
                                let key = k.to_vec();
                                if !overlay.contains_key(&key) {
                                    condemned.push(key);
                                }
                            }
                        }
                        None => {
                            for item in db.range(start.as_slice()..) {
                                let (k, _) = item.map_err(store_err)?;
                                let key = k.to_vec();
                                if !key.starts_with(&start) {
                                    break;
                                }
                                if !overlay.contains_key(&key) {
                                    condemned.push(key);
                                }
                            }
                        }
                    }
                    for key in condemned {
                        batch.remove(key.clone());
                        overlay.insert(key, None);
                    }
                }
            }
        }
        db.apply_batch(batch).map_err(store_err)?;
        match durability {
            // Force the WAL to stable storage before returning.
            Durability::SyncWal => {
                db.flush().map_err(store_err)?;
            }
            // sled has no WAL-without-fsync level and no WAL-disable knob:
            // batches still go through the WAL, background flush decides
            // when bytes hit disk. Documented, same as upstream behavior.
            Durability::WalOnly | Durability::DisableWal => {}
        }
        Ok(())
    }
}
