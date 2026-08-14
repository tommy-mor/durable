//! Event-sourced application-state runtime.
//!
//! The log is the source of truth. The RocksDB projection is rebuildable.
//! Python (or any other client) may append a recognized event and issue a
//! serializable query. It cannot write the projection directly.
//!
//! ```text
//! append(Event) → log (fsync) → reducer(&mut Tx) → projection + offset
//! query(Path)   → query engine → Value
//! rebuild()     → destroy projection → reducer(event 0..n)
//! verify()      → incremental == replay from zero
//! ```

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write as IoWrite};
use std::marker::PhantomData;
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::query::{self, Plan, Query};
use crate::shape::field_segment;
use crate::{codec, Batch, Db, Describe, Durability, Error, Leaf, Path as DPath, Result, Shape, Write};
use ciborium::Value;

const META_NS: &str = "__durable";

/// A reducer transaction: reified writes plus committed-state reads.
///
/// Writes in this transaction are not visible to [`Tx::db`] until commit.
/// Put the event's writes and the projection offset in the same batch so a
/// crash before commit simply replays the event against the previous state.
pub struct Tx {
    batch: Batch,
}

impl Tx {
    fn new(db: &Db) -> Self {
        Self { batch: db.batch() }
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
        let line = serde_json::to_string(event).map_err(|e| Error::Serialize(e.to_string()))?;
        writeln!(&self.file, "{line}").map_err(|e| Error::Log(e.to_string()))?;
        // Content durability. Metadata (mtime) is not the tape.
        self.file.sync_data().map_err(|e| Error::Log(e.to_string()))?;
        self.len += 1;
        Ok(idx)
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
                let event: E = serde_json::from_str(&line)
                    .map_err(|e| Error::Deserialize(e.to_string()))?;
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

/// Event-sourced runtime: canonical log + reducer + rebuildable projection.
pub struct Runtime<E> {
    log: JsonlLog<E>,
    db: Db,
    schema: Shape,
    namespace: Option<String>,
    reducer: fn(&mut Tx, &E) -> Result<()>,
    /// Events this process has applied. Loaded from the projection on open.
    applied: u64,
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
        let mut rt = Self {
            log: JsonlLog::open(log_path)?,
            db,
            schema,
            namespace,
            reducer,
            applied,
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

    pub fn log(&self) -> &JsonlLog<E> {
        &self.log
    }

    /// How many events the projection has applied.
    pub fn applied(&self) -> Result<u64> {
        Ok(self.applied)
    }

    /// Append an event to the log, then reduce it into the projection.
    ///
    /// The log is fsynced first. Projection writes and the applied offset
    /// commit in one RocksDB batch, so a crash before that commit replays
    /// cleanly against the previous projection.
    pub fn append(&mut self, event: E) -> Result<u64> {
        self.catch_up()?;
        let idx = self.log.append(&event)?;
        self.apply_event(idx, &event)?;
        Ok(idx)
    }

    /// Replay any log events the projection has not yet applied.
    pub fn catch_up(&mut self) -> Result<u64> {
        let log_len = self.log.len()?;
        if self.applied >= log_len {
            return Ok(0);
        }
        let pending = self.log.read_from(self.applied)?;
        let n = pending.len() as u64;
        for (idx, event) in pending {
            self.apply_event(idx, &event)?;
        }
        Ok(n)
    }

    fn apply_event(&mut self, idx: u64, event: &E) -> Result<()> {
        let mut tx = Tx::new(&self.db);
        (self.reducer)(&mut tx, event)?;
        tx.write(applied_leaf().set(&(idx + 1)));
        // The log is the durable source of truth; the projection is rebuildable.
        tx.commit(Durability::DisableWal)?;
        self.applied = idx + 1;
        Ok(())
    }

    /// Drop the projection and rebuild it from event 0.
    pub fn rebuild(&mut self) -> Result<()> {
        let mut tx = Tx::new(&self.db);
        tx.write(Write::new(crate::Op::DeletePrefix {
            prefix: Vec::new(),
        }));
        tx.commit(Durability::DisableWal)?;
        self.applied = 0;
        self.catch_up()?;
        Ok(())
    }

    /// Property: the live projection equals a fresh replay from zero.
    pub fn verify(&self) -> Result<()> {
        let tmp = tempfile_dir()?;
        let db_path = tmp.join("proj");
        let mut other = Runtime::open(
            &db_path,
            self.log.path(),
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
        .filter(|line| {
            line.as_ref()
                .map(|l| !l.trim().is_empty())
                .unwrap_or(false)
        })
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

