//! `redb`-native storage engine: MVCC snapshots (`ReadTx`), read-your-writes
//! write transactions (`Tx`), and lazy double-ended streaming range scans.

use std::ops::Bound;
use std::path::Path as FsPath;
use std::sync::Arc;

use redb::{
    Database, ReadableDatabase as _, ReadableTable as _, ReadableTableMetadata as _, TableDefinition,
    TableError, TableHandle as _,
};

use crate::watch::{table_in_namespace, WatchRange};
use crate::{codec, Durability, Error, Meta, Result};

pub(crate) const BOOKKEEPING_TABLE: &str = "__durable";
pub(crate) const SCALARS_TABLE: &str = "__scalars";

fn table_def(name: &str) -> TableDefinition<'_, &'static [u8], &'static [u8]> {
    TableDefinition::new(name)
}

fn store_err<E: std::fmt::Display>(e: E) -> Error {
    Error::Store(e.to_string())
}

fn open_read_table(
    txn: &redb::ReadTransaction,
    name: &str,
) -> Result<Option<redb::ReadOnlyTable<&'static [u8], &'static [u8]>>> {
    match txn.open_table(table_def(name)) {
        Ok(t) => Ok(Some(t)),
        Err(TableError::TableDoesNotExist(_)) => Ok(None),
        Err(e) => Err(store_err(e)),
    }
}

fn bound_as_slice(b: &Bound<Vec<u8>>) -> Bound<&[u8]> {
    match b {
        Bound::Included(v) => Bound::Included(v.as_slice()),
        Bound::Excluded(v) => Bound::Excluded(v.as_slice()),
        Bound::Unbounded => Bound::Unbounded,
    }
}

/// A handle to an open `redb`-backed durable database.
///
/// Cheap to clone (`Arc<redb::Database>`). Concurrent [`ReadTx`] snapshots
/// never block writers and never observe partial commits.
#[derive(Clone)]
pub struct Db {
    inner: Arc<Database>,
}

impl Db {
    /// Open (or create) a `redb` database inside directory `path`
    /// (`<path>/store.redb`).
    pub fn open<P: AsRef<FsPath>>(path: P) -> Result<Self> {
        let dir = path.as_ref();
        std::fs::create_dir_all(dir).map_err(|e| Error::Log(e.to_string()))?;
        let db = Database::create(dir.join("store.redb")).map_err(store_err)?;
        Ok(Self {
            inner: Arc::new(db),
        })
    }

    /// Open an ephemeral in-memory `redb` database backed by
    /// `redb::backends::InMemoryBackend`. Runs the exact same B-tree engine as
    /// on-disk databases with zero filesystem I/O.
    pub fn open_memory() -> Result<Self> {
        let db = Database::builder()
            .create_with_backend(redb::backends::InMemoryBackend::new())
            .map_err(store_err)?;
        Ok(Self {
            inner: Arc::new(db),
        })
    }

    /// Begin a snapshot-consistent read transaction.
    ///
    /// Every read through the returned [`ReadTx`] observes the same atomic
    /// point-in-time snapshot, and range iterators stream lazily in either
    /// direction without materializing the range into memory.
    pub fn read(&self) -> Result<ReadTx> {
        let txn = self.inner.begin_read().map_err(store_err)?;
        Ok(ReadTx { txn })
    }

    /// Begin a read-your-writes transaction with [`Durability::SyncWal`].
    pub fn write(&self) -> Result<Tx> {
        self.write_with(Durability::SyncWal)
    }

    /// Begin a read-your-writes transaction with an explicit [`Durability`].
    pub fn write_with(&self, durability: Durability) -> Result<Tx> {
        let mut txn = self.inner.begin_write().map_err(store_err)?;
        txn.set_durability(match durability {
            Durability::SyncWal | Durability::WalOnly => redb::Durability::Immediate,
            Durability::DisableWal => redb::Durability::None,
        })
        .map_err(store_err)?;
        Ok(Tx {
            txn,
            meta: Meta::default(),
            touched: Vec::new(),
        })
    }

    /// Run `f` inside a single atomic write transaction, committing on `Ok` and
    /// aborting on `Err`.
    pub fn txn<T>(&self, durability: Durability, f: impl FnOnce(&mut Tx) -> Result<T>) -> Result<T> {
        let mut tx = self.write_with(durability)?;
        let out = f(&mut tx)?;
        tx.commit()?;
        Ok(out)
    }

    /// Snapshot-consistent sorted dump of every table: `(table, key, value)`.
    pub fn dump(&self) -> Result<Vec<(String, Vec<u8>, Vec<u8>)>> {
        let r = self.read()?;
        r.dump()
    }
}

/// Lazy, double-ended byte-range iterator over a single table.
#[doc(hidden)]
pub struct StoreRange(StoreRangeInner);

enum StoreRangeInner {
    Empty,
    Redb(redb::Range<'static, &'static [u8], &'static [u8]>),
    Buffered(std::vec::IntoIter<(Vec<u8>, Vec<u8>)>),
}

impl StoreRange {
    pub(crate) fn empty() -> Self {
        Self(StoreRangeInner::Empty)
    }

    pub(crate) fn redb(iter: redb::Range<'static, &'static [u8], &'static [u8]>) -> Self {
        Self(StoreRangeInner::Redb(iter))
    }

    pub(crate) fn buffered(iter: std::vec::IntoIter<(Vec<u8>, Vec<u8>)>) -> Self {
        Self(StoreRangeInner::Buffered(iter))
    }
}

impl Iterator for StoreRange {
    type Item = Result<(Vec<u8>, Vec<u8>)>;

    fn next(&mut self) -> Option<Self::Item> {
        match &mut self.0 {
            StoreRangeInner::Empty => None,
            StoreRangeInner::Redb(iter) => iter.next().map(|res| {
                res.map(|(k, v)| (k.value().to_vec(), v.value().to_vec()))
                    .map_err(store_err)
            }),
            StoreRangeInner::Buffered(iter) => iter.next().map(Ok),
        }
    }
}

impl DoubleEndedIterator for StoreRange {
    fn next_back(&mut self) -> Option<Self::Item> {
        match &mut self.0 {
            StoreRangeInner::Empty => None,
            StoreRangeInner::Redb(iter) => iter.next_back().map(|res| {
                res.map(|(k, v)| (k.value().to_vec(), v.value().to_vec()))
                    .map_err(store_err)
            }),
            StoreRangeInner::Buffered(iter) => iter.next_back().map(Ok),
        }
    }
}

/// Read capability shared by [`ReadTx`], [`Tx`] (read-your-writes), and [`Db`]
/// (convenience one-off reads).
pub trait Read {
    #[doc(hidden)]
    fn raw_get(&self, table: &str, key: &[u8]) -> Result<Option<Vec<u8>>>;

    #[doc(hidden)]
    fn raw_table_len(&self, table: &str) -> Result<u64>;

    #[doc(hidden)]
    fn raw_first_in(
        &self,
        table: &str,
        lo: Bound<Vec<u8>>,
        hi: Bound<Vec<u8>>,
    ) -> Result<Option<(Vec<u8>, Vec<u8>)>>;

    #[doc(hidden)]
    fn raw_last_in(
        &self,
        table: &str,
        lo: Bound<Vec<u8>>,
        hi: Bound<Vec<u8>>,
    ) -> Result<Option<(Vec<u8>, Vec<u8>)>>;

    #[doc(hidden)]
    fn raw_range(
        &self,
        table: &str,
        lo: Bound<Vec<u8>>,
        hi: Bound<Vec<u8>>,
    ) -> Result<StoreRange>;

    #[doc(hidden)]
    fn raw_count_in(&self, table: &str, lo: Bound<Vec<u8>>, hi: Bound<Vec<u8>>) -> Result<u64>;
}

/// A snapshot-consistent read transaction over a [`Db`].
///
/// Open one per HTTP request or query batch (`let r = rt.read()?;` or
/// `let r = db.read()?;`) so every point read and range scan observes one
/// consistent snapshot with zero extra transaction setup cost.
pub struct ReadTx {
    txn: redb::ReadTransaction,
}

impl ReadTx {
    /// Snapshot-consistent sorted dump of every table: `(table, key, value)`.
    pub fn dump(&self) -> Result<Vec<(String, Vec<u8>, Vec<u8>)>> {
        let mut names: Vec<String> = self
            .txn
            .list_tables()
            .map_err(store_err)?
            .map(|h| h.name().to_string())
            .collect();
        names.sort();
        let mut out = Vec::new();
        for name in names {
            let Some(table) = open_read_table(&self.txn, &name)? else {
                continue;
            };
            for item in table.range::<&[u8]>(..).map_err(store_err)? {
                let (k, v) = item.map_err(store_err)?;
                out.push((name.clone(), k.value().to_vec(), v.value().to_vec()));
            }
        }
        Ok(out)
    }
}

impl Read for ReadTx {
    fn raw_get(&self, table: &str, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let Some(t) = open_read_table(&self.txn, table)? else {
            return Ok(None);
        };
        Ok(t.get(key).map_err(store_err)?.map(|g| g.value().to_vec()))
    }

    fn raw_table_len(&self, table: &str) -> Result<u64> {
        let Some(t) = open_read_table(&self.txn, table)? else {
            return Ok(0);
        };
        t.len().map_err(store_err)
    }

    fn raw_first_in(
        &self,
        table: &str,
        lo: Bound<Vec<u8>>,
        hi: Bound<Vec<u8>>,
    ) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        let Some(t) = open_read_table(&self.txn, table)? else {
            return Ok(None);
        };
        let mut iter = t
            .range::<&[u8]>((bound_as_slice(&lo), bound_as_slice(&hi)))
            .map_err(store_err)?;
        match iter.next() {
            Some(Ok((k, v))) => Ok(Some((k.value().to_vec(), v.value().to_vec()))),
            Some(Err(e)) => Err(store_err(e)),
            None => Ok(None),
        }
    }

    fn raw_last_in(
        &self,
        table: &str,
        lo: Bound<Vec<u8>>,
        hi: Bound<Vec<u8>>,
    ) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        let Some(t) = open_read_table(&self.txn, table)? else {
            return Ok(None);
        };
        let mut iter = t
            .range::<&[u8]>((bound_as_slice(&lo), bound_as_slice(&hi)))
            .map_err(store_err)?;
        match iter.next_back() {
            Some(Ok((k, v))) => Ok(Some((k.value().to_vec(), v.value().to_vec()))),
            Some(Err(e)) => Err(store_err(e)),
            None => Ok(None),
        }
    }

    fn raw_range(
        &self,
        table: &str,
        lo: Bound<Vec<u8>>,
        hi: Bound<Vec<u8>>,
    ) -> Result<StoreRange> {
        let Some(t) = open_read_table(&self.txn, table)? else {
            return Ok(StoreRange::empty());
        };
        // Inherent `ReadOnlyTable::range` returns `Range<'static, ...>` backed
        // by an `Arc<TransactionGuard>` — lazy, streaming, double-ended.
        let iter = t
            .range::<&[u8]>((bound_as_slice(&lo), bound_as_slice(&hi)))
            .map_err(store_err)?;
        Ok(StoreRange::redb(iter))
    }

    fn raw_count_in(&self, table: &str, lo: Bound<Vec<u8>>, hi: Bound<Vec<u8>>) -> Result<u64> {
        let Some(t) = open_read_table(&self.txn, table)? else {
            return Ok(0);
        };
        if matches!(lo, Bound::Unbounded) && matches!(hi, Bound::Unbounded) {
            return t.len().map_err(store_err);
        }
        let iter = t
            .range::<&[u8]>((bound_as_slice(&lo), bound_as_slice(&hi)))
            .map_err(store_err)?;
        let mut count = 0u64;
        for item in iter {
            let _ = item.map_err(store_err)?;
            count += 1;
        }
        Ok(count)
    }
}

/// Convenience one-off reads directly on `&Db`.
///
/// Each call opens a short-lived snapshot; for multi-read consistency and speed,
/// prefer `let r = db.read()?;` (or `rt.read()?`).
impl Read for Db {
    fn raw_get(&self, table: &str, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.read()?.raw_get(table, key)
    }

    fn raw_table_len(&self, table: &str) -> Result<u64> {
        self.read()?.raw_table_len(table)
    }

    fn raw_first_in(
        &self,
        table: &str,
        lo: Bound<Vec<u8>>,
        hi: Bound<Vec<u8>>,
    ) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        self.read()?.raw_first_in(table, lo, hi)
    }

    fn raw_last_in(
        &self,
        table: &str,
        lo: Bound<Vec<u8>>,
        hi: Bound<Vec<u8>>,
    ) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        self.read()?.raw_last_in(table, lo, hi)
    }

    fn raw_range(
        &self,
        table: &str,
        lo: Bound<Vec<u8>>,
        hi: Bound<Vec<u8>>,
    ) -> Result<StoreRange> {
        self.read()?.raw_range(table, lo, hi)
    }

    fn raw_count_in(&self, table: &str, lo: Bound<Vec<u8>>, hi: Bound<Vec<u8>>) -> Result<u64> {
        self.read()?.raw_count_in(table, lo, hi)
    }
}

/// An atomic read-your-writes transaction wrapping `redb::WriteTransaction`.
///
/// Inside a [`crate::Runtime`] reducer (`fn(&mut Tx, &E) -> Result<()>`),
/// mutations are immediately visible to subsequent reads on the same `Tx`,
/// applied strictly in arrival order, and rolled back automatically if the
/// reducer returns `Err` (before the event ever touches the tape).
pub struct Tx {
    txn: redb::WriteTransaction,
    meta: Meta,
    touched: Vec<WatchRange>,
}

impl Tx {
    /// Sequence number and timestamp of the event currently being reduced
    /// (zeroed when opened standalone via [`Db::write`]).
    pub fn meta(&self) -> Meta {
        self.meta
    }

    /// Log index of the event currently being reduced.
    pub fn seq(&self) -> u64 {
        self.meta.seq
    }

    pub(crate) fn set_meta(&mut self, meta: Meta) {
        self.meta = meta;
    }

    /// The key ranges touched by mutations in this transaction so far.
    pub fn touched(&self) -> &[WatchRange] {
        &self.touched
    }

    /// Commit the transaction atomically, returning the touched [`WatchRange`]s.
    pub fn commit(self) -> Result<Vec<WatchRange>> {
        let touched = self.touched;
        self.txn.commit().map_err(store_err)?;
        Ok(touched)
    }

    /// Abort and discard all writes made in this transaction.
    pub fn abort(self) -> Result<()> {
        self.txn.abort().map_err(store_err)
    }

    pub(crate) fn raw_put(&mut self, table: &str, key: &[u8], value: &[u8]) -> Result<()> {
        {
            let mut t = self.txn.open_table(table_def(table)).map_err(store_err)?;
            t.insert(key, value).map_err(store_err)?;
        }
        if table != BOOKKEEPING_TABLE {
            self.touched.push(WatchRange::point(table, key));
        }
        Ok(())
    }

    pub(crate) fn raw_delete(&mut self, table: &str, key: &[u8]) -> Result<bool> {
        let existed = {
            let mut t = self.txn.open_table(table_def(table)).map_err(store_err)?;
            let found = t.remove(key).map_err(store_err)?.is_some();
            found
        };
        if table != BOOKKEEPING_TABLE {
            self.touched.push(WatchRange::point(table, key));
        }
        Ok(existed)
    }

    pub(crate) fn raw_delete_prefix(&mut self, table: &str, prefix: &[u8]) -> Result<()> {
        if prefix.is_empty() {
            return self.raw_delete_table(table);
        }
        let end = codec::prefix_upper_bound(prefix);
        {
            let mut t = self.txn.open_table(table_def(table)).map_err(store_err)?;
            match &end {
                Some(hi) => {
                    t.retain_in(prefix..hi.as_slice(), |_, _| false)
                        .map_err(store_err)?;
                }
                None => {
                    t.retain_in(prefix.., |_, _| false).map_err(store_err)?;
                }
            }
        }
        if table != BOOKKEEPING_TABLE {
            self.touched.push(WatchRange::prefix(table, prefix));
        }
        Ok(())
    }

    pub(crate) fn raw_delete_table(&mut self, table: &str) -> Result<()> {
        self.txn
            .delete_table(table_def(table))
            .map_err(store_err)?;
        if table != BOOKKEEPING_TABLE {
            self.touched.push(WatchRange::table(table));
        }
        Ok(())
    }

    /// Delete every application table (leaving `__durable` bookkeeping untouched).
    pub(crate) fn raw_wipe_app(&mut self) -> Result<()> {
        let handles: Vec<_> = self
            .txn
            .list_tables()
            .map_err(store_err)?
            .filter(|h| h.name() != BOOKKEEPING_TABLE)
            .collect();
        for h in handles {
            self.txn.delete_table(h).map_err(store_err)?;
        }
        self.touched.push(WatchRange::all());
        Ok(())
    }

    /// Delete every table in a namespace (`"{ns}/..."`).
    pub(crate) fn raw_wipe_namespace(&mut self, ns: &str) -> Result<()> {
        let handles: Vec<_> = self
            .txn
            .list_tables()
            .map_err(store_err)?
            .filter(|h| table_in_namespace(h.name(), ns))
            .collect();
        for h in handles {
            self.txn.delete_table(h).map_err(store_err)?;
        }
        self.touched.push(WatchRange::namespace(ns));
        Ok(())
    }

    /// Delete every table in the database, including `__durable` bookkeeping.
    pub(crate) fn raw_wipe_everything(&mut self) -> Result<()> {
        let handles: Vec<_> = self.txn.list_tables().map_err(store_err)?.collect();
        for h in handles {
            self.txn.delete_table(h).map_err(store_err)?;
        }
        self.touched.push(WatchRange::all());
        Ok(())
    }

    /// Pop the smallest key-value pair in `[lo, hi)`.
    pub(crate) fn raw_pop_first_in(
        &mut self,
        table: &str,
        lo: Bound<Vec<u8>>,
        hi: Bound<Vec<u8>>,
    ) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        let popped = {
            let mut t = self.txn.open_table(table_def(table)).map_err(store_err)?;
            if matches!(lo, Bound::Unbounded) && matches!(hi, Bound::Unbounded) {
                t.pop_first()
                    .map_err(store_err)?
                    .map(|(k, v)| (k.value().to_vec(), v.value().to_vec()))
            } else {
                let first = {
                    let mut iter = t
                        .range::<&[u8]>((bound_as_slice(&lo), bound_as_slice(&hi)))
                        .map_err(store_err)?;
                    let res = match iter.next() {
                        Some(Ok((k, v))) => Some((k.value().to_vec(), v.value().to_vec())),
                        Some(Err(e)) => return Err(store_err(e)),
                        None => None,
                    };
                    res
                };
                if let Some((ref k, _)) = first {
                    t.remove(k.as_slice()).map_err(store_err)?;
                }
                first
            }
        };
        if let Some((ref k, _)) = popped {
            if table != BOOKKEEPING_TABLE {
                self.touched.push(WatchRange::point(table, k));
            }
        }
        Ok(popped)
    }

    /// Pop the largest key-value pair in `[lo, hi)`.
    pub(crate) fn raw_pop_last_in(
        &mut self,
        table: &str,
        lo: Bound<Vec<u8>>,
        hi: Bound<Vec<u8>>,
    ) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        let popped = {
            let mut t = self.txn.open_table(table_def(table)).map_err(store_err)?;
            if matches!(lo, Bound::Unbounded) && matches!(hi, Bound::Unbounded) {
                t.pop_last()
                    .map_err(store_err)?
                    .map(|(k, v)| (k.value().to_vec(), v.value().to_vec()))
            } else {
                let last = {
                    let mut iter = t
                        .range::<&[u8]>((bound_as_slice(&lo), bound_as_slice(&hi)))
                        .map_err(store_err)?;
                    let res = match iter.next_back() {
                        Some(Ok((k, v))) => Some((k.value().to_vec(), v.value().to_vec())),
                        Some(Err(e)) => return Err(store_err(e)),
                        None => None,
                    };
                    res
                };
                if let Some((ref k, _)) = last {
                    t.remove(k.as_slice()).map_err(store_err)?;
                }
                last
            }
        };
        if let Some((ref k, _)) = popped {
            if table != BOOKKEEPING_TABLE {
                self.touched.push(WatchRange::point(table, k));
            }
        }
        Ok(popped)
    }
}

impl Read for Tx {
    fn raw_get(&self, table: &str, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let t = self.txn.open_table(table_def(table)).map_err(store_err)?;
        let out = t.get(key).map_err(store_err)?.map(|g| g.value().to_vec());
        Ok(out)
    }

    fn raw_table_len(&self, table: &str) -> Result<u64> {
        let t = self.txn.open_table(table_def(table)).map_err(store_err)?;
        t.len().map_err(store_err)
    }

    fn raw_first_in(
        &self,
        table: &str,
        lo: Bound<Vec<u8>>,
        hi: Bound<Vec<u8>>,
    ) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        let t = self.txn.open_table(table_def(table)).map_err(store_err)?;
        let mut iter = t
            .range::<&[u8]>((bound_as_slice(&lo), bound_as_slice(&hi)))
            .map_err(store_err)?;
        let out = match iter.next() {
            Some(Ok((k, v))) => Ok(Some((k.value().to_vec(), v.value().to_vec()))),
            Some(Err(e)) => Err(store_err(e)),
            None => Ok(None),
        };
        out
    }

    fn raw_last_in(
        &self,
        table: &str,
        lo: Bound<Vec<u8>>,
        hi: Bound<Vec<u8>>,
    ) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        let t = self.txn.open_table(table_def(table)).map_err(store_err)?;
        let mut iter = t
            .range::<&[u8]>((bound_as_slice(&lo), bound_as_slice(&hi)))
            .map_err(store_err)?;
        let out = match iter.next_back() {
            Some(Ok((k, v))) => Ok(Some((k.value().to_vec(), v.value().to_vec()))),
            Some(Err(e)) => Err(store_err(e)),
            None => Ok(None),
        };
        out
    }

    fn raw_range(
        &self,
        table: &str,
        lo: Bound<Vec<u8>>,
        hi: Bound<Vec<u8>>,
    ) -> Result<StoreRange> {
        let t = self.txn.open_table(table_def(table)).map_err(store_err)?;
        let iter = t
            .range::<&[u8]>((bound_as_slice(&lo), bound_as_slice(&hi)))
            .map_err(store_err)?;
        let mut out = Vec::new();
        for item in iter {
            let (k, v) = item.map_err(store_err)?;
            out.push((k.value().to_vec(), v.value().to_vec()));
        }
        Ok(StoreRange::buffered(out.into_iter()))
    }

    fn raw_count_in(&self, table: &str, lo: Bound<Vec<u8>>, hi: Bound<Vec<u8>>) -> Result<u64> {
        let t = self.txn.open_table(table_def(table)).map_err(store_err)?;
        if matches!(lo, Bound::Unbounded) && matches!(hi, Bound::Unbounded) {
            return t.len().map_err(store_err);
        }
        let iter = t
            .range::<&[u8]>((bound_as_slice(&lo), bound_as_slice(&hi)))
            .map_err(store_err)?;
        let mut count = 0u64;
        for item in iter {
            let _ = item.map_err(store_err)?;
            count += 1;
        }
        Ok(count)
    }
}
