//! Event-sourced application-state runtime.
//!
//! The log is the source of truth. The RocksDB projection is rebuildable.
//! Python (or any other client) may append a recognized event and issue a
//! serializable query. It cannot write the projection directly.
//!
//! Ingest is owned by this process. Callers supply an event body; the runtime
//! stamps a sequence number and a monotonic timestamp onto a [`Record`] before
//! the line hits the tape. Replay uses those stamped fields, never the clock.
//!
//! ```text
//! append(Event) → Record{seq, ts_ms, event} → log (fsync) → reducer(&mut Tx) → projection + offset
//! query(Path)   → query engine → Value
//! rebuild()     → destroy projection → reducer(event 0..n)
//! verify()      → incremental == replay from zero
//! ```
//!
//! Writes take an internal mutex. Queries read RocksDB without it, so an
//! fsync does not block `one` / `select`. [`Runtime::append_batch`] fsyncs
//! once per batch (group commit).

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write as IoWrite};
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::query::{self, Plan, Query};
use crate::shape::field_segment;
use crate::{
    codec, Batch, Db, Describe, Durability, Error, Leaf, Path as DPath, Result, Shape, Write,
};
use ciborium::Value;

const META_NS: &str = "__durable";

/// Authoritative ingest metadata. Assigned by [`Runtime`] at append time.
///
/// `seq` is the 0-based index on the tape. `ts_ms` is a monotonic millisecond
/// timestamp (wall clock, but never steps backwards within this process).
/// A reducer that needs an id or a time reads this — it does not trust the
/// event body to have minted either.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Meta {
    pub seq: u64,
    pub ts_ms: u64,
}

/// One fsynced log line: ingest metadata plus the event body.
///
/// The body is nested (not flattened) so an event field named `seq` or
/// `ts_ms` cannot collide with the envelope.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record<E> {
    pub seq: u64,
    pub ts_ms: u64,
    pub event: E,
}

impl<E> Record<E> {
    pub fn meta(&self) -> Meta {
        Meta {
            seq: self.seq,
            ts_ms: self.ts_ms,
        }
    }
}

/// A reducer transaction: reified writes plus committed-state reads.
///
/// Writes in this transaction are not visible to [`Tx::db`] until commit.
/// Put the event's writes and the projection offset in the same batch so a
/// crash before commit simply replays the event against the previous state.
pub struct Tx {
    batch: Batch,
    meta: Meta,
}

impl Tx {
    fn new(db: &Db, meta: Meta) -> Self {
        Self {
            batch: db.batch(),
            meta,
        }
    }

    /// Sequence number and timestamp stamped at ingest (or recovered from the tape).
    pub fn meta(&self) -> Meta {
        self.meta
    }

    /// Record a reified write. This is how a reducer mutates state.
    pub fn write(&mut self, write: Write) {
        self.batch.write(write);
    }

    /// Record several reified writes.
    pub fn extend<I: IntoIterator<Item = Write>>(&mut self, writes: I) {
        self.batch.extend(writes);
    }

    /// Committed projection, not including in-flight writes of this event.
    pub fn db(&self) -> &Db {
        self.batch.db()
    }

    fn commit(self, durability: Durability) -> Result<()> {
        self.batch.commit_with(durability)
    }
}

/// Append-only JSONL event log. One object per line; the file is canonical.
///
/// Length is counted once at open and maintained by [`JsonlLog::append`].
/// This process is the writer; do not append to the same file from elsewhere
/// and expect `len` to notice.
pub struct JsonlLog<E> {
    path: PathBuf,
    file: File,
    len: u64,
    _event: PhantomData<E>,
}

impl<E> JsonlLog<E> {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| Error::Log(e.to_string()))?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(&path)
            .map_err(|e| Error::Log(e.to_string()))?;
        let len = count_jsonl_lines(&mut file)?;
        Ok(Self {
            path,
            file,
            len,
            _event: PhantomData,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn len(&self) -> Result<u64> {
        Ok(self.len)
    }

    pub fn is_empty(&self) -> Result<bool> {
        Ok(self.len == 0)
    }
}

impl<E: Serialize + DeserializeOwned> JsonlLog<E> {
    /// Append one event, fsync the file contents, return its 0-based index.
    pub fn append(&mut self, event: &E) -> Result<u64> {
        let idx = self.len;
        self.append_all(std::slice::from_ref(event))?;
        Ok(idx)
    }

    /// Append `events` as consecutive lines and fsync once. Returns the index
    /// of the first event (equal to `len` before the call). Empty input is a
    /// no-op and does not fsync.
    pub fn append_all(&mut self, events: &[E]) -> Result<u64> {
        let start = self.len;
        if events.is_empty() {
            return Ok(start);
        }
        for event in events {
            let line = serde_json::to_string(event).map_err(|e| Error::Serialize(e.to_string()))?;
            writeln!(&self.file, "{line}").map_err(|e| Error::Log(e.to_string()))?;
        }
        // Content durability. Metadata (mtime) is not the tape.
        self.file
            .sync_data()
            .map_err(|e| Error::Log(e.to_string()))?;
        self.len += events.len() as u64;
        Ok(start)
    }

    /// Events from `start` (inclusive) to the end, with their indices.
    pub fn read_from(&self, start: u64) -> Result<Vec<(u64, E)>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let file = File::open(&self.path).map_err(|e| Error::Log(e.to_string()))?;
        let mut out = Vec::new();
        let mut idx = 0u64;
        for line in BufReader::new(file).lines() {
            let line = line.map_err(|e| Error::Log(e.to_string()))?;
            if line.trim().is_empty() {
                continue;
            }
            if idx >= start {
                let event: E =
                    serde_json::from_str(&line).map_err(|e| Error::Deserialize(e.to_string()))?;
                out.push((idx, event));
            }
            idx += 1;
        }
        Ok(out)
    }

    pub fn read_all(&self) -> Result<Vec<(u64, E)>> {
        self.read_from(0)
    }
}

fn applied_leaf() -> DPath<Leaf<u64>> {
    let mut prefix = Vec::new();
    codec::put_segment(&mut prefix, META_NS.as_bytes());
    DPath::from_prefix(codec::child_key(&prefix, &field_segment(0)))
}

struct Inner<E> {
    log: JsonlLog<Record<E>>,
    applied: u64,
    last_ts_ms: u64,
}

/// Event-sourced runtime: canonical log + reducer + rebuildable projection.
///
/// `append` / `append_batch` serialize writers through an internal mutex.
/// Query methods (`one`, `select`, …) only touch the RocksDB handle, which
/// is safe to share with an in-flight fsync.
pub struct Runtime<E> {
    inner: Mutex<Inner<E>>,
    applied: AtomicU64,
    db: Db,
    schema: Shape,
    namespace: Option<String>,
    reducer: fn(&mut Tx, &E) -> Result<()>,
}

impl<E: Serialize + DeserializeOwned> Runtime<E> {
    /// Open (or create) a runtime and catch the projection up to the log.
    pub fn open(
        db_path: impl AsRef<Path>,
        log_path: impl AsRef<Path>,
        schema: Shape,
        namespace: Option<String>,
        reducer: fn(&mut Tx, &E) -> Result<()>,
    ) -> Result<Self> {
        let db_path = db_path.as_ref().to_path_buf();
        if let Some(parent) = db_path.parent() {
            fs::create_dir_all(parent).map_err(|e| Error::Log(e.to_string()))?;
        }
        let db = Db::open(&db_path)?;
        let applied = applied_leaf().get(&db)?.unwrap_or(0);
        let log = JsonlLog::open(log_path)?;
        let last_ts_ms = max_ts_on_tape(&log)?;
        let rt = Self {
            inner: Mutex::new(Inner {
                log,
                applied,
                last_ts_ms,
            }),
            applied: AtomicU64::new(applied),
            db,
            schema,
            namespace,
            reducer,
        };
        rt.catch_up()?;
        Ok(rt)
    }

    /// Convenience: schema from a [`Describe`] type.
    pub fn open_described<S: Describe>(
        db_path: impl AsRef<Path>,
        log_path: impl AsRef<Path>,
        namespace: Option<String>,
        reducer: fn(&mut Tx, &E) -> Result<()>,
    ) -> Result<Self> {
        Self::open(db_path, log_path, S::shape(), namespace, reducer)
    }

    pub fn db(&self) -> &Db {
        &self.db
    }

    pub fn schema(&self) -> &Shape {
        &self.schema
    }

    /// Path of the canonical JSONL tape.
    pub fn log_path(&self) -> Result<PathBuf> {
        let inner = self
            .inner
            .lock()
            .map_err(|_| Error::Log("runtime lock poisoned".into()))?;
        Ok(inner.log.path().to_path_buf())
    }

    /// How many lines are on the tape.
    pub fn log_len(&self) -> Result<u64> {
        let inner = self
            .inner
            .lock()
            .map_err(|_| Error::Log("runtime lock poisoned".into()))?;
        inner.log.len()
    }

    /// How many events the projection has applied.
    pub fn applied(&self) -> Result<u64> {
        Ok(self.applied.load(Ordering::Acquire))
    }

    /// Records on the tape from `start` (inclusive) to the end.
    pub fn records_from(&self, start: u64) -> Result<Vec<Record<E>>> {
        let inner = self
            .inner
            .lock()
            .map_err(|_| Error::Log("runtime lock poisoned".into()))?;
        Ok(inner
            .log
            .read_from(start)?
            .into_iter()
            .map(|(_, rec)| rec)
            .collect())
    }

    /// Append an event to the log, then reduce it into the projection.
    ///
    /// The runtime stamps [`Meta`] (seq, monotonic ts_ms). The log is fsynced
    /// first. Projection writes and the applied offset commit in one RocksDB
    /// batch, so a crash before that commit replays cleanly against the
    /// previous projection.
    pub fn append(&self, event: E) -> Result<Record<E>> {
        let mut recs = self.append_batch(vec![event])?;
        recs.pop()
            .ok_or_else(|| Error::Log("append_batch returned empty".into()))
    }

    /// Append `events` as one group commit: one fsync, then reduce each.
    ///
    /// Empty input is a no-op. Seq numbers are contiguous. Timestamps are
    /// monotonic across the batch and with previous ingest.
    pub fn append_batch(&self, events: Vec<E>) -> Result<Vec<Record<E>>> {
        if events.is_empty() {
            return Ok(Vec::new());
        }
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| Error::Log("runtime lock poisoned".into()))?;
        self.catch_up_locked(&mut inner)?;
        let mut records = Vec::with_capacity(events.len());
        let mut seq = inner.log.len()?;
        let mut ts_ms = inner.last_ts_ms;
        for event in events {
            ts_ms = next_ts(ts_ms);
            records.push(Record { seq, ts_ms, event });
            seq += 1;
        }
        inner.log.append_all(&records)?;
        inner.last_ts_ms = ts_ms;
        for rec in &records {
            self.apply_record(&mut inner, rec)?;
        }
        Ok(records)
    }

    /// Replay any log events the projection has not yet applied.
    pub fn catch_up(&self) -> Result<u64> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| Error::Log("runtime lock poisoned".into()))?;
        self.catch_up_locked(&mut inner)
    }

    fn catch_up_locked(&self, inner: &mut Inner<E>) -> Result<u64> {
        let log_len = inner.log.len()?;
        if inner.applied >= log_len {
            return Ok(0);
        }
        let pending = inner.log.read_from(inner.applied)?;
        let n = pending.len() as u64;
        for (_, rec) in pending {
            inner.last_ts_ms = inner.last_ts_ms.max(rec.ts_ms);
            self.apply_record(inner, &rec)?;
        }
        Ok(n)
    }

    fn apply_record(&self, inner: &mut Inner<E>, rec: &Record<E>) -> Result<()> {
        let mut tx = Tx::new(&self.db, rec.meta());
        (self.reducer)(&mut tx, &rec.event)?;
        tx.write(applied_leaf().set(&(rec.seq + 1)));
        // The log is the durable source of truth; the projection is rebuildable.
        tx.commit(Durability::DisableWal)?;
        inner.applied = rec.seq + 1;
        self.applied.store(inner.applied, Ordering::Release);
        Ok(())
    }

    /// Drop the projection and rebuild it from event 0.
    pub fn rebuild(&self) -> Result<()> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| Error::Log("runtime lock poisoned".into()))?;
        let mut tx = Tx::new(&self.db, Meta { seq: 0, ts_ms: 0 });
        tx.write(Write::new(crate::Op::DeletePrefix { prefix: Vec::new() }));
        tx.commit(Durability::DisableWal)?;
        inner.applied = 0;
        self.applied.store(0, Ordering::Release);
        self.catch_up_locked(&mut inner)?;
        Ok(())
    }

    /// Property: the live projection equals a fresh replay from zero.
    pub fn verify(&self) -> Result<()> {
        let inner = self
            .inner
            .lock()
            .map_err(|_| Error::Log("runtime lock poisoned".into()))?;
        let log_path = inner.log.path().to_path_buf();
        drop(inner);
        let tmp = tempfile_dir()?;
        let db_path = tmp.join("proj");
        let other = Runtime::open(
            &db_path,
            &log_path,
            self.schema.clone(),
            self.namespace.clone(),
            self.reducer,
        )?;
        other.rebuild()?;
        let left = dump(&self.db)?;
        let right = dump(&other.db)?;
        drop(other);
        let _ = fs::remove_dir_all(&tmp);
        if left != right {
            return Err(Error::Query(format!(
                "verify failed: incremental ({} keys) != replay ({} keys)",
                left.len(),
                right.len()
            )));
        }
        Ok(())
    }

    pub fn one(&self, query: &Query) -> Result<Option<Value>> {
        query::one(&self.db, &self.schema, &self.scoped(query))
    }

    pub fn select(&self, query: &Query) -> Result<Vec<Value>> {
        query::select(&self.db, &self.schema, &self.scoped(query))
    }

    pub fn subtree(&self, query: &Query) -> Result<Value> {
        query::subtree(&self.db, &self.schema, &self.scoped(query))
    }

    pub fn entries(&self, query: &Query) -> Result<Vec<(Value, Value)>> {
        query::entries(&self.db, &self.schema, &self.scoped(query))
    }

    pub fn project(&self, spec: &[(String, Query)]) -> Result<Value> {
        let scoped: Vec<(String, Query)> = spec
            .iter()
            .map(|(k, q)| (k.clone(), self.scoped(q)))
            .collect();
        query::project(&self.db, &self.schema, &scoped)
    }

    pub fn explain(&self, query: &Query) -> Result<Plan> {
        query::explain(&self.schema, &self.scoped(query))
    }

    fn scoped(&self, query: &Query) -> Query {
        let mut q = query.clone();
        if q.namespace.is_none() {
            q.namespace = self.namespace.clone();
        }
        q
    }
}

fn max_ts_on_tape<E: Serialize + DeserializeOwned>(log: &JsonlLog<Record<E>>) -> Result<u64> {
    let mut max = 0u64;
    for (_, rec) in log.read_all()? {
        max = max.max(rec.ts_ms);
    }
    Ok(max)
}

fn next_ts(last: u64) -> u64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    now.max(last.saturating_add(1))
}

fn dump(db: &Db) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let iter = db.raw().iterator(rocksdb::IteratorMode::Start);
    let mut out = Vec::new();
    for item in iter {
        let (k, v) = item?;
        out.push((k.to_vec(), v.to_vec()));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

fn count_jsonl_lines(file: &mut File) -> Result<u64> {
    file.seek(SeekFrom::Start(0))
        .map_err(|e| Error::Log(e.to_string()))?;
    let count = BufReader::new(&*file)
        .lines()
        .filter(|line| line.as_ref().map(|l| !l.trim().is_empty()).unwrap_or(false))
        .count() as u64;
    file.seek(SeekFrom::End(0))
        .map_err(|e| Error::Log(e.to_string()))?;
    Ok(count)
}

fn tempfile_dir() -> Result<PathBuf> {
    let base = std::env::temp_dir().join(format!(
        "durable-verify-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    fs::create_dir_all(&base).map_err(|e| Error::Log(e.to_string()))?;
    Ok(base)
}
