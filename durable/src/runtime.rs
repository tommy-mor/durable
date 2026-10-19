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
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::dynpath;
use crate::query::{self, Nav, Plan, Query};
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
/// `ts_ms` cannot collide with the envelope. `from`/`to` name this record's
/// batch (a lone append has `from == seq == to`); `crc` seals the line so a
/// torn tail reads as uncommitted by rule, never by judgment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record<E> {
    pub seq: u64,
    pub ts_ms: u64,
    pub from: u64,
    pub to: u64,
    pub event: E,
    pub crc: String,
}

impl<E: Serialize> Serialize for Record<E> {
    fn serialize<S: serde::Serializer>(
        &self,
        s: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut st = s.serialize_struct("Record", 6)?;
        st.serialize_field("seq", &self.seq)?;
        st.serialize_field("ts_ms", &self.ts_ms)?;
        st.serialize_field("from", &self.from)?;
        st.serialize_field("to", &self.to)?;
        st.serialize_field("event", &self.event)?;
        st.serialize_field("crc", &self.crc)?;
        st.end()
    }
}

impl<'de, E: DeserializeOwned> Deserialize<'de> for Record<E> {
    fn deserialize<D: serde::Deserializer<'de>>(
        d: D,
    ) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Helper<E> {
            seq: u64,
            ts_ms: u64,
            // No legacy fallback: an unsealed record has no from/to/crc and
            // must not deserialize as if it were sealed.
            from: u64,
            to: u64,
            event: E,
            crc: String,
        }
        let h = Helper::deserialize(d)?;
        Ok(Record {
            seq: h.seq,
            ts_ms: h.ts_ms,
            from: h.from,
            to: h.to,
            event: h.event,
            crc: h.crc,
        })
    }
}
impl<E> Record<E> {
    pub fn meta(&self) -> Meta {
        Meta {
            seq: self.seq,
            ts_ms: self.ts_ms,
        }
    }
}

/// CRC32C over line bytes as written, rendered as 8 lowercase hex digits.
/// `crc32c` is Castagnoli, not the `crc32fast` IEEE already in the tree.
pub fn seal_crc(prefix_bytes: &[u8]) -> String {
    format!("{:08x}", crc32c::crc32c(prefix_bytes))
}

/// Render one sealed tape line: compact JSON, `seq` first for
/// `grep '^{"seq":…`, envelope then body, CRC suffix always exactly
/// `,"crc":"xxxxxxxx"}` (18 bytes) so checking is slicing, never
/// canonical-JSON. The only raw `\n` is added by the writer.
pub fn render_sealed<E: Serialize>(seq: u64, ts_ms: u64, from: u64, to: u64, event: &E) -> Result<String> {
    let body =
        serde_json::to_string(event).map_err(|e| Error::Serialize(e.to_string()))?;
    let mut prefix = format!(
        "{{\"seq\":{seq},\"ts_ms\":{ts_ms},\"from\":{from},\"to\":{to},\"event\":{body}"
    );
    let crc = seal_crc(prefix.as_bytes());
    prefix.push_str(",\"crc\":\"");
    prefix.push_str(&crc);
    prefix.push_str("\"}");
    Ok(prefix)
}

/// A checked sealed line: envelope fields plus the raw event bytes, so the
/// caller can decode `E` itself. Rejects overlong lines, bad CRC, and bad
/// envelope shapes without ever hunting JSON fragments inside damage.
#[derive(Debug, Clone, Copy)]
pub struct SealedEnvelope {
    pub seq: u64,
    pub ts_ms: u64,
    pub from: u64,
    pub to: u64,
}

/// Maximum sealed line: 16 MiB per the spec; anything longer is not a
/// candidate line, no matter how it parses.
pub const MAX_SEALED_LINE: usize = 16 * 1024 * 1024;
/// Largest batch the scan will call complete: bounds `to - from`.
pub const MAX_SEALED_BATCH: u64 = 1024;
/// CRC suffix length: `,"crc":"xxxxxxxx"}` is always exactly 18 bytes.
pub const CRC_SUFFIX_LEN: usize = 18;

/// What the scan found: the committed prefix ends at byte `end`, holds
/// `len` records, and the highest ingest timestamp among them is `last_ts`.
/// Everything past `end` is in-flight residue, never committed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Committed {
    pub end: usize,
    pub len: u64,
    pub last_ts: u64,
}

/// Where the scan stopped believing: byte `offset` and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Corruption {
    pub offset: usize,
    pub reason: String,
}

/// One pure function decides where the tape ends. Used by the writer at
/// open, by in-process readers, by `verify` and by the CLI — nothing else
/// may judge the bytes. A torn tail drops by rule (incomplete batch past
/// the committed prefix); a valid line past it with a foreign `from`
/// proves acknowledged bytes were damaged and is fatal.
pub fn scan(bytes: &[u8]) -> std::result::Result<Committed, Corruption> {
    let mut committed = Committed {
        end: 0,
        len: 0,
        last_ts: 0,
    };
    // Outside: between batches. Inside: mid-batch, expecting `next`.
    let mut inside: Option<(u64, u64, u64)> = None;
    let mut bad: Option<(usize, String)> = None;
    // Physical lines: split on \n, terminator included in length math.
    let mut start = 0usize;
    while start <= bytes.len() {
        let rel = bytes[start..].iter().position(|b| *b == b'\n');
        let (chunk, next) = match rel {
            Some(i) => (&bytes[start..start + i], start + i + 1),
            None => (&bytes[start..], bytes.len() + 1),
        };
        let terminated = next <= bytes.len();
        let offset = start;
        if !terminated {
            // Trailing bytes with no newline: torn write, stop here.
            if !chunk.iter().all(|b| b.is_ascii_whitespace()) && !chunk.is_empty() {
                bad = Some((offset, "torn tail without newline".into()));
            }
            break;
        }
        // Blank lines are skipped, legacy compatibility.
        if chunk.iter().all(|b| b.is_ascii_whitespace()) {
            start = next;
            continue;
        }
        let (env, _event) = match parse_sealed(chunk) {
            Ok(v) => v,
            Err(e) => {
                bad = Some((offset, format!("not a candidate line: {e}")));
                break;
            }
        };
        // Candidate-line envelope rules already checked part; require
        // batch coherence against the scan state.
        match inside {
            None => {
                if env.from != committed.len || env.seq != env.from {
                    bad = Some((
                        offset,
                        format!(
                            "batch opens at from={} seq={} but tape holds {}",
                            env.from, env.seq, committed.len
                        ),
                    ));
                    break;
                }
                inside = Some((env.from, env.to, env.seq + 1));
            }
            Some((from, to, next_seq)) => {
                if env.from != from || env.to != to || env.seq != next_seq {
                    bad = Some((
                        offset,
                        format!(
                            "batch [{from}..{to}] expects seq {next_seq} but got seq {}",
                            env.seq
                        ),
                    ));
                    break;
                }
                inside = Some((from, to, next_seq + 1));
            }
        }
        if env.seq == env.to {
            committed = Committed {
                end: next,
                len: env.to + 1,
                last_ts: committed.last_ts.max(env.ts_ms),
            };
            inside = None;
        }
        start = next;
    }
    if let Some((off, reason)) = bad {
        // Tail check: every candidate line past committed.end with
        // from == committed.len is in-flight residue, even a surviving
        // closing line. Any other from proves a later batch was written,
        // so the bad bytes are damage to acknowledged data: fatal.
        let mut pos = committed.end;
        while pos < bytes.len() {
            let rel = bytes[pos..].iter().position(|b| *b == b'\n');
            let (chunk, next) = match rel {
                Some(i) => (&bytes[pos..pos + i], pos + i + 1),
                None => break,
            };
            if !chunk.iter().all(|b| b.is_ascii_whitespace()) && !chunk.is_empty() {
                match parse_sealed(chunk) {
                    Ok((env, _)) => {
                        if env.from != committed.len {
                            return Err(Corruption { offset: off, reason });
                        }
                    }
                    Err(_) if is_unsealed_json(chunk) => {
                        // A complete line that is valid JSON but carries no
                        // crc suffix can't come from the sealed writer: a
                        // torn write never ends in `}\n`, and zero-filled
                        // pages are not valid JSON. It is someone else's
                        // history (an unmigrated tape, a hand edit), never
                        // residue. Refuse; never truncate it away.
                        return Err(Corruption {
                            offset: pos,
                            reason: "unsealed line: not written by the sealed writer \
                                     (migrate old tapes offline before opening)"
                                .into(),
                        });
                    }
                    Err(_) => {}
                }
            }
            pos = next;
        }
        // Residue only: the committed prefix stands.
        return Ok(committed);
    }
    Ok(committed)
}

/// Valid JSON without the sealed `,"crc":"xxxxxxxx"}` suffix shape. A sealed
/// line whose CRC no longer matches still has the suffix shape and stays
/// ordinary damage; only lines the sealed writer could never have produced
/// match here.
fn is_unsealed_json(line: &[u8]) -> bool {
    let has_crc_suffix = line.len() >= CRC_SUFFIX_LEN
        && line[line.len() - CRC_SUFFIX_LEN..].starts_with(b",\"crc\":\"")
        && line.ends_with(b"\"}");
    !has_crc_suffix && serde_json::from_slice::<serde::de::IgnoredAny>(line).is_ok()
}

pub fn parse_sealed(line: &[u8]) -> Result<(SealedEnvelope, Vec<u8>)> {
    if line.len() > MAX_SEALED_LINE {
        return Err(Error::Deserialize("sealed line over 16 MiB".into()));
    }
    if line.len() < CRC_SUFFIX_LEN + 2 {
        return Err(Error::Deserialize("sealed line too short for a crc".into()));
    }
    let (prefix, suffix) = line.split_at(line.len() - CRC_SUFFIX_LEN);
    if !suffix.starts_with(b",\"crc\":\"") || !suffix.ends_with(b"\"}") {
        return Err(Error::Deserialize("sealed line missing crc suffix".into()));
    }
    let got = std::str::from_utf8(&suffix[8..16])
        .map_err(|e| Error::Deserialize(e.to_string()))?;
    if !got.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Error::Deserialize("sealed crc not hex".into()));
    }
    let want = seal_crc(prefix);
    if got != want {
        return Err(Error::Deserialize("sealed crc mismatch".into()));
    }
    let env: SealedEnvelopeWire =
        serde_json::from_slice(line).map_err(|e| Error::Deserialize(e.to_string()))?;
    if env.from > env.to || env.seq < env.from || env.seq > env.to {
        return Err(Error::Deserialize("sealed envelope seq outside batch".into()));
    }
    if env.to - env.from >= MAX_SEALED_BATCH {
        return Err(Error::Deserialize("sealed batch too large".into()));
    }
    let event_bytes = env.event.get().as_bytes().to_vec();
    Ok((
        SealedEnvelope {
            seq: env.seq,
            ts_ms: env.ts_ms,
            from: env.from,
            to: env.to,
        },
        event_bytes,
    ))
}

#[derive(Deserialize)]
struct SealedEnvelopeWire<'a> {
    seq: u64,
    ts_ms: u64,
    from: u64,
    to: u64,
    #[serde(borrow)]
    event: &'a serde_json::value::RawValue,
}

/// A reducer transaction: reified writes plus committed-state reads.
///
/// Writes in this transaction are not visible to [`Tx::db`] until commit.
/// Put the event's writes and the projection offset in the same batch so a
/// crash before commit simply replays the event against the previous state.
pub struct Tx {
    batch: Batch,
    meta: Meta,
    /// Every reified write recorded, kept alongside the batch so the
    /// commit can report what it touched without re-reading anything.
    writes: Vec<Write>,
}

impl Tx {
    fn new(db: &Db, meta: Meta) -> Self {
        Self {
            batch: db.batch(),
            meta,
            writes: Vec::new(),
        }
    }

    /// Sequence number and timestamp stamped at ingest (or recovered from the tape).
    pub fn meta(&self) -> Meta {
        self.meta
    }

    /// Log index of the event being reduced. Stable across replay: it is
    /// the line number in the JSONL tape. Use this for server-assigned
    /// ids — never take ids from the browser.
    pub fn seq(&self) -> u64 {
        self.meta.seq
    }

    /// Dynamic put: a path-as-data write against `schema`.
    pub fn put(&mut self, schema: &Shape, navs: &[Nav], value: &Value) -> Result<()> {
        self.extend(dynpath::put(schema, None, navs, value)?);
        Ok(())
    }

    /// Dynamic delete / subtree delete.
    pub fn delete(&mut self, schema: &Shape, navs: &[Nav]) -> Result<()> {
        self.write(dynpath::delete(schema, None, navs)?);
        Ok(())
    }

    /// Dynamic Sum add (blind merge).
    pub fn add(&mut self, schema: &Shape, navs: &[Nav], delta: &Value) -> Result<()> {
        self.write(dynpath::add(schema, None, navs, delta)?);
        Ok(())
    }

    /// Dynamic list/deque append.
    pub fn push(&mut self, schema: &Shape, navs: &[Nav], value: &Value) -> Result<()> {
        self.write(dynpath::push(schema, None, navs, value)?);
        Ok(())
    }

    /// Dynamic collection clear.
    pub fn clear(&mut self, schema: &Shape, navs: &[Nav]) -> Result<()> {
        self.write(dynpath::clear(schema, None, navs)?);
        Ok(())
    }

    /// Read committed state (not in-flight writes of this event) at `navs`.
    pub fn peek(&self, schema: &Shape, navs: &[Nav]) -> Result<Value> {
        dynpath::peek(self.db(), schema, None, navs)
    }

    /// Record a reified write. This is how a reducer mutates state.
    pub fn write(&mut self, write: Write) {
        self.batch.write(write.clone());
        self.writes.push(write);
    }

    /// Record several reified writes.
    pub fn extend<I: IntoIterator<Item = Write>>(&mut self, writes: I) {
        for w in writes {
            self.write(w);
        }
    }

    /// Every write this transaction recorded — the commit's waking set.
    pub fn writes(&self) -> &[Write] {
        &self.writes
    }

    /// Committed projection, not including in-flight writes of this event.
    pub fn db(&self) -> &Db {
        self.batch.db()
    }

    fn commit(self, durability: Durability) -> Result<Vec<Write>> {
        let writes = self.writes.clone();
        self.batch.commit_with(durability)?;
        Ok(writes)
    }
}

/// Append-only JSONL event log. One object per line; the file is canonical.
///
/// Length is counted once at open and maintained by [`JsonlLog::append`].
/// This process is the writer; do not append to the same file from elsewhere
/// and expect `len` to notice.
pub struct JsonlLog<E> {
    path: PathBuf,
    file: Box<dyn TapeFile>,
    len: u64,
    /// Byte offset of the committed end. Everything past it on disk is
    /// residue of a batch that never committed.
    end: u64,
    /// Set when an I/O failure left the tape in a state this process can't
    /// vouch for. Every later append refuses until the tape is reopened,
    /// which re-derives the truth from the bytes.
    poisoned: bool,
    _event: PhantomData<E>,
}

/// Handle indirection for the tape file. Production wraps a real [`File`];
/// tests wrap a failing double. The writer only touches the tape through
/// this trait, so every outcome-table row is reachable without real ENOSPC.
pub trait TapeFile: Send {
    fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()>;
    fn sync_data(&self) -> std::io::Result<()>;
    fn file_len(&self) -> std::io::Result<u64>;
    fn set_len(&self, len: u64) -> std::io::Result<()>;
}

impl TapeFile for File {
    fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
        IoWrite::write_all(self, buf)
    }
    fn sync_data(&self) -> std::io::Result<()> {
        File::sync_data(self)
    }
    fn file_len(&self) -> std::io::Result<u64> {
        Ok(self.metadata()?.len())
    }
    fn set_len(&self, len: u64) -> std::io::Result<()> {
        File::set_len(self, len)
    }
}

/// A tape file that fails on command. Test double for the writer outcome
/// table: the poison, rollback, and shrink-guard branches are otherwise
/// unreachable without real ENOSPC or EIO. Only built with the
/// `fault-injection` feature, which the crate's own tests turn on.
#[cfg(feature = "fault-injection")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailMode {
    /// Every write returns ENOSPC having landed nothing.
    WriteEnospc,
    /// Every write lands its first `n` bytes, then returns ENOSPC: what a
    /// full disk does mid-batch.
    WritePartialEnospc(usize),
    /// Partial write as above AND truncate returns EPERM: the partial
    /// bytes can't be rolled back.
    WritePartialEnospcAndTruncateEperm(usize),
    /// Every sync returns EIO (Linux may mark pages clean).
    SyncEio,
    /// The next sync returns EIO, later syncs succeed: batch sync fails
    /// but rollback's sync lands — the NotCommitted-poisoned row.
    SyncEioOnce,
    /// Truncate returns EPERM: rollback cannot be confirmed.
    TruncateEperm,
    /// Sync returns EIO AND truncate returns EPERM: rollback
    /// unconfirmable after a failed commit — the Unknown row.
    SyncEioAndTruncateEperm,
    /// Length reports a shrunken file: acknowledged bytes vanished.
    ShrunkTo(u64),
}

/// Failing test double over an in-memory disk. Writes append to the disk,
/// `set_len` really truncates it, and the test keeps a handle to the disk
/// so it can scan exactly what a rollback left behind.
#[cfg(feature = "fault-injection")]
#[derive(Debug)]
pub struct FailingFile {
    disk: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    mode: Option<FailMode>,
    syncs_failed: std::cell::Cell<usize>,
}

/// Shared view of a [`FailingFile`]'s disk bytes.
#[cfg(feature = "fault-injection")]
pub type FakeDisk = std::sync::Arc<std::sync::Mutex<Vec<u8>>>;

#[cfg(feature = "fault-injection")]
impl FailingFile {
    /// A double whose disk starts as `bytes`. Returns the double and a
    /// handle to its disk.
    pub fn over(bytes: Vec<u8>, mode: Option<FailMode>) -> (Self, FakeDisk) {
        let disk = std::sync::Arc::new(std::sync::Mutex::new(bytes));
        (
            Self {
                disk: disk.clone(),
                mode,
                syncs_failed: std::cell::Cell::new(0),
            },
            disk,
        )
    }

    fn truncate_fails(&self) -> bool {
        matches!(
            self.mode,
            Some(FailMode::TruncateEperm)
                | Some(FailMode::SyncEioAndTruncateEperm)
                | Some(FailMode::WritePartialEnospcAndTruncateEperm(_))
        )
    }
}

#[cfg(feature = "fault-injection")]
impl TapeFile for FailingFile {
    fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
        let mut disk = self.disk.lock().unwrap();
        match self.mode {
            Some(FailMode::WriteEnospc) => Err(std::io::Error::from_raw_os_error(28)),
            Some(FailMode::WritePartialEnospc(n))
            | Some(FailMode::WritePartialEnospcAndTruncateEperm(n)) => {
                disk.extend_from_slice(&buf[..n.min(buf.len())]);
                Err(std::io::Error::from_raw_os_error(28))
            }
            _ => {
                disk.extend_from_slice(buf);
                Ok(())
            }
        }
    }
    fn sync_data(&self) -> std::io::Result<()> {
        match self.mode {
            Some(FailMode::SyncEio) | Some(FailMode::SyncEioAndTruncateEperm) => {
                Err(std::io::Error::from_raw_os_error(5))
            }
            Some(FailMode::SyncEioOnce) if self.syncs_failed.get() == 0 => {
                self.syncs_failed.set(1);
                Err(std::io::Error::from_raw_os_error(5))
            }
            _ => Ok(()),
        }
    }
    fn file_len(&self) -> std::io::Result<u64> {
        if let Some(FailMode::ShrunkTo(n)) = self.mode {
            return Ok(n);
        }
        Ok(self.disk.lock().unwrap().len() as u64)
    }
    fn set_len(&self, len: u64) -> std::io::Result<()> {
        if self.truncate_fails() {
            return Err(std::io::Error::from_raw_os_error(1));
        }
        self.disk.lock().unwrap().resize(len as usize, 0);
        Ok(())
    }
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
        let end = file.metadata().map_err(|e| Error::Log(e.to_string()))?.len();
        Ok(Self {
            path,
            file: Box::new(file),
            len,
            end,
            poisoned: false,
            _event: PhantomData,
        })
    }

    /// Seal-gated open for sealed tapes: scan read-only, truncate
    /// in-flight residue, sync unconditionally, then count. Truncation here
    /// is not a repair — it removes only bytes the format says were never
    /// committed, is idempotent across interrupted opens, and is required
    /// so the next O_APPEND batch never lands behind dead bytes.
    /// Returns the log plus what the scan found.
    pub fn open_sealed(path: impl AsRef<Path>) -> Result<(Self, Committed)> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| Error::Log(e.to_string()))?;
        }
        if !path.exists() {
            let log = Self::open(&path)?;
            return Ok((log, Committed { end: 0, len: 0, last_ts: 0 }));
        }
        // Spec order: scan read-only FIRST, cross-check (by the caller)
        // against the projection, and only then seal. Truncating before
        // the check would destroy the evidence disagreement needs.
        let found = Self::scan_file(&path)?;
        Self::seal_to(&path, &found)?;
        let mut log = Self::open(&path)?;
        log.len = found.len;
        log.end = found.end as u64;
        Ok((log, found))
    }

    /// Read-only scan of the tape file: no mutation, file untouched on
    /// both committed and corrupt outcomes. The cross-check reads this
    /// before anything is sealed.
    pub fn scan_file(path: &Path) -> Result<Committed> {
        let bytes = fs::read(path).map_err(|e| Error::Log(e.to_string()))?;
        scan(&bytes).map_err(|c| {
            Error::Corruption(format!("tape corrupt at byte {}: {}", c.offset, c.reason))
        })
    }
    /// Seal the tape to the scanned committed end: truncate in-flight
    /// residue, then sync unconditionally. Truncation is not a repair —
    /// it removes only bytes the format says were never committed, is
    /// idempotent across interrupted opens, and keeps the next O_APPEND
    /// batch from landing behind dead bytes.
    /// There is no legacy path: an unsealed line makes the scan refuse, so
    /// it never reaches here. Old tapes migrate through an offline
    /// migration script, never through the writer.
    pub fn seal_to(path: &Path, found: &Committed) -> Result<()> {
        let f = OpenOptions::new()
            .write(true)
            .open(path)
            .map_err(|e| Error::Log(e.to_string()))?;
        let len = f.metadata().map_err(|e| Error::Log(e.to_string()))?.len() as usize;
        if found.end < len {
            f.set_len(found.end as u64)
                .map_err(|e| Error::Log(e.to_string()))?;
            eprintln!(
                "tape: dropped {} uncommitted bytes past seq {}",
                len - found.end,
                found.len
            );
        }
        // Unconditional: a restarted process may be reading a complete batch
        // a crashed writer left in the page cache, never confirmed durable.
        // Nothing past this point may treat the prefix as committed until
        // this sync succeeds.
        f.sync_data().map_err(|e| Error::Log(e.to_string()))?;
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn len(&self) -> Result<u64> {
        Ok(self.len)
    }

    /// Whether an I/O failure poisoned this writer. A poisoned writer
    /// refuses every append until the tape is reopened, which re-derives
    /// the truth from the bytes. Test hook for the outcome table.
    pub fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    /// Swap the live file handle for a failing test double. The writer
    /// keeps its committed end, so the double's disk should start as the
    /// real tape's bytes. Test seam only (`fault-injection` feature).
    #[cfg(feature = "fault-injection")]
    pub fn inject_failing_file(&mut self, fail: FailingFile) {
        self.file = Box::new(fail);
    }

    pub fn is_empty(&self) -> Result<bool> {
        Ok(self.len == 0)
    }
}

impl<E> JsonlLog<E> {
    /// Cut the file back to the committed end and sync. If either step
    /// fails the writer can no longer vouch for the file: poison it.
    fn rollback(&mut self) -> Result<()> {
        let res = self
            .file
            .set_len(self.end)
            .and_then(|_| self.file.sync_data());
        if let Err(e) = res {
            self.poisoned = true;
            return Err(Error::Log(format!(
                "rollback to committed end failed (writer poisoned): {e}"
            )));
        }
        Ok(())
    }
}

impl<E: Serialize + DeserializeOwned> JsonlLog<Record<E>> {
    /// Append sealed records as consecutive lines and fsync once. Returns
    /// the index of the first record (equal to `len` before the call).
    /// Empty input is a no-op and does not fsync.
    pub fn append_all(&mut self, events: &[Record<E>]) -> Result<u64> {
        let start = self.len;
        if events.is_empty() {
            return Ok(start);
        }
        // Render the whole batch first, sealing every line's CRC fresh, so a
        // serialization failure never drools half a thrust onto the tape.
        let mut batch = String::with_capacity(events.len() * 256);
        for rec in events {
            let line = render_sealed(rec.seq, rec.ts_ms, rec.from, rec.to, &rec.event)?;
            batch.push_str(&line);
            batch.push('\n');
        }
        if self.poisoned {
            return Err(Error::Log(
                "tape writer poisoned by an earlier I/O failure; reopen the tape".into(),
            ));
        }
        // O(1) guard, no rescan: this process owns the committed end. Bytes
        // past it are residue of a batch that never committed; cut them so
        // the next batch never lands behind dead bytes. A file shorter than
        // the committed end means acknowledged bytes vanished under us.
        let on_disk = self
            .file
            .file_len()
            .map_err(|e| Error::Log(e.to_string()))?;
        if on_disk < self.end {
            self.poisoned = true;
            return Err(Error::Corruption(format!(
                "tape shrank under writer ({on_disk} bytes < committed end {})",
                self.end
            )));
        }
        if on_disk > self.end {
            self.rollback()?;
        }
        if let Err(e) = self.file.write_all(batch.as_bytes()) {
            // The closing line's newline can't have landed, so the scan
            // drops this batch regardless; rolling back keeps O_APPEND clean.
            self.rollback()?;
            return Err(Error::Log(format!("not committed: {e}")));
        }
        // Content durability. Metadata (mtime) is not the tape.
        if let Err(e) = self.file.sync_data() {
            // The bytes may still reach disk as a closed batch, and Linux
            // may already have marked the dirty pages clean: never retry the
            // fsync and trust it. Roll back, and stop writing either way.
            let rolled = self.rollback().is_ok();
            self.poisoned = true;
            return Err(Error::Log(if rolled {
                format!("not committed (writer poisoned): {e}")
            } else {
                format!("commit outcome unknown (writer poisoned): {e}")
            }));
        }
        self.end += batch.len() as u64;
        self.len += events.len() as u64;
        Ok(start)
    }

    /// Sealed records from `start` (inclusive) to the end, with their tape
    /// indices. Every line is checked through `parse_sealed`; a torn tail
    /// is the scan's business, never this reader's — here a bad line is a
    /// hard error, because readers only walk the committed prefix.
    pub fn read_from(&self, start: u64) -> Result<Vec<(u64, Record<E>)>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let bytes = fs::read(&self.path).map_err(|e| Error::Log(e.to_string()))?;
        let mut out = Vec::new();
        let mut idx = 0u64;
        for chunk in bytes.split(|b| *b == b'\n') {
            if chunk.is_empty() {
                continue;
            }
            // Skip whitespace-only legacy padding without paying parse.
            if chunk.iter().all(|b| b.is_ascii_whitespace()) {
                continue;
            }
            let (env, event_bytes) = parse_sealed(chunk)?;
            if idx >= start {
                let event: E = serde_json::from_slice(&event_bytes)
                    .map_err(|e| Error::Deserialize(e.to_string()))?;
                let crc = String::from_utf8_lossy(
                    &chunk[chunk.len() - CRC_SUFFIX_LEN + 8..chunk.len() - 2],
                )
                .into_owned();
                out.push((
                    idx,
                    Record {
                        seq: env.seq,
                        ts_ms: env.ts_ms,
                        from: env.from,
                        to: env.to,
                        event,
                        crc,
                    },
                ));
            }
            idx += 1;
        }
        Ok(out)
    }

    pub fn read_all(&self) -> Result<Vec<(u64, Record<E>)>> {
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
    reducer: Arc<dyn Fn(&mut Tx, &E) -> Result<()> + Send + Sync>,
    /// Fired after every committed event with the seq and the writes it
    /// touched. The courier hangs her waking loop here — single-threaded
    /// runtimes call it inline on the committing thread.
    on_commit: Mutex<Option<Arc<dyn Fn(u64, &[Write]) + Send + Sync>>>,
    /// Writes of the most recent committed event. Stashed under the ingest
    /// lock; the store binding pulls them right after `append` returns to
    /// wake parked waiters — same thread, no race.
    last_writes: Mutex<Vec<Write>>,
}

impl<E: Serialize + DeserializeOwned + 'static> Runtime<E> {
    /// Open (or create) a runtime and catch the projection up to the log.
    pub fn open(
        db_path: impl AsRef<Path>,
        log_path: impl AsRef<Path>,
        schema: Shape,
        namespace: Option<String>,
        reducer: impl Fn(&mut Tx, &E) -> Result<()> + Send + Sync + 'static,
    ) -> Result<Self> {
        let db_path = db_path.as_ref().to_path_buf();
        if let Some(parent) = db_path.parent() {
            fs::create_dir_all(parent).map_err(|e| Error::Log(e.to_string()))?;
        }
        let log_path = log_path.as_ref().to_path_buf();
        if let Some(parent) = log_path.parent() {
            fs::create_dir_all(parent).map_err(|e| Error::Log(e.to_string()))?;
        }
        // Spec Opening order: scan read-only, open the projection, then
        // cross-check BEFORE sealing. Sealing first would destroy the
        // evidence disagreement needs — the file must stay byte-identical
        // on every fatal path.
        let found = if log_path.exists() {
            JsonlLog::<Record<E>>::scan_file(&log_path)?
        } else {
            Committed { end: 0, len: 0, last_ts: 0 }
        };
        // No legacy path, ever: an unsealed line is refused by the scan
        // with the file untouched, never dropped as residue. Old tapes
        // migrate through the offline migration script, never the writer.
        let db = Db::open(&db_path)?;
        let applied = applied_leaf().get(&db)?.unwrap_or(0);
        if applied > found.len {
            return Err(Error::Corruption(format!(
                "projection and tape disagree (applied={applied}, tape has {})",
                found.len
            )));
        }
        if log_path.exists() {
            JsonlLog::<Record<E>>::seal_to(&log_path, &found)?;
        }
        let mut log = JsonlLog::open(&log_path)?;
        log.len = found.len;
        log.end = found.end as u64;
        let last_ts_ms = found.last_ts;
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
            reducer: Arc::new(reducer),
            on_commit: Mutex::new(None),
            last_writes: Mutex::new(Vec::new()),
        };
        rt.catch_up()?;
        Ok(rt)
    }

    /// Convenience: schema from a [`Describe`] type.
    pub fn open_described<S: Describe>(
        db_path: impl AsRef<Path>,
        log_path: impl AsRef<Path>,
        namespace: Option<String>,
        reducer: impl Fn(&mut Tx, &E) -> Result<()> + Send + Sync + 'static,
    ) -> Result<Self> {
        Self::open(db_path, log_path, S::shape(), namespace, reducer)
    }

    pub fn db(&self) -> &Db {
        &self.db
    }

    pub fn schema(&self) -> &Shape {
        &self.schema
    }

    /// Hang the courier here: called after every committed event with its
    /// seq and the writes it touched. Replaces any previous listener.
    pub fn on_commit(&self, f: impl Fn(u64, &[Write]) + Send + Sync + 'static) {
        *self.on_commit.lock().expect("on_commit lock") = Some(Arc::new(f));
    }

    /// Drop the commit listener.
    pub fn clear_on_commit(&self) {
        *self.on_commit.lock().expect("on_commit lock") = None;
    }

    /// Writes of the most recent committed event — the waking set the
    /// courier checks parked watches against.
    pub fn last_writes(&self) -> Vec<Write> {
        self.last_writes.lock().expect("last_writes lock").clone()
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
        if events.len() as u64 > MAX_SEALED_BATCH {
            return Err(Error::Log(format!(
                "batch of {} exceeds sealed maximum {MAX_SEALED_BATCH}",
                events.len()
            )));
        }
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| Error::Log("runtime lock poisoned".into()))?;
        self.catch_up_locked(&mut inner)?;
        let mut records = Vec::with_capacity(events.len());
        let from = inner.log.len()?;
        let to = from + events.len() as u64 - 1;
        let mut seq = from;
        let mut ts_ms = inner.last_ts_ms;
        for event in events {
            ts_ms = next_ts(ts_ms);
            records.push(Record {
                seq,
                ts_ms,
                from,
                to,
                event,
                crc: String::new(),
            });
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
        let writes = tx.commit(Durability::DisableWal)?;
        *self.last_writes.lock().expect("last_writes lock") = writes.clone();
        inner.applied = rec.seq + 1;
        self.applied.store(inner.applied, Ordering::Release);
        // Wake whoever waits: report the commit's waking set AFTER it is
        // durable, while still holding the ingest lock so no waiter can
        // register between the commit and this fire.
        let listener = self.on_commit.lock().expect("on_commit lock").clone();
        if let Some(f) = listener {
            f(rec.seq, &writes);
        }
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
        // Hold the ingest lock for the whole replay. The replay opens the
        // same tape, and its open may truncate residue past the committed
        // end — which, during a live append, would be the batch being
        // written. With the lock held no batch is in flight.
        let _ingest = inner;
        let tmp = tempfile_dir()?;
        let db_path = tmp.join("proj");
        let reducer = self.reducer.clone();
        let other = Runtime::open(
            &db_path,
            &log_path,
            self.schema.clone(),
            self.namespace.clone(),
            move |tx, ev| reducer(tx, ev),
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

    /// Whether any non-null value exists at or under the path.
    pub fn has(&self, query: &Query) -> Result<bool> {
        query::has(&self.db, &self.schema, &self.scoped(query))
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

fn next_ts(last: u64) -> u64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    now.max(last.saturating_add(1))
}

fn dump(db: &Db) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let iter = db.iter_start()?;
    let mut out = Vec::new();
    for item in iter {
        out.push(item?);
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
