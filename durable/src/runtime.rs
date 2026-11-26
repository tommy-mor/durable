//! Event-sourced application-state runtime: self-sealing JSONL tape + typed
//! `redb` projection.
//!
//! ```text
//! WRITE:  Event(s) → reduce into redb WriteTx (validated, uncommitted)
//!                  → render + fsync sealed JSONL tape
//!                  → commit WriteTx atomically (+ applied offset + idempotency key)
//!                  → notify typed watch subscribers
//! READ:   rt.read()? → MVCC ReadTx snapshot → Path::get / range / prefix / page / len
//! ```
//!
//! - A reducer rejection (`Err`) aborts the `WriteTx` **before** anything hits
//!   the tape — poison events never land on the tape.
//! - Multi-event batches (`append_batch`) run in one `WriteTx` with full
//!   read-your-writes across events and become visible to readers atomically.
//! - All tape scanning and catch-up stream line-by-line (O(1) RAM) and seek
//!   from `applied_byte_offset` (O(delta) catch-up).
//! - Replay commits in chunks of [`REPLAY_CHUNK_SIZE`] events per `redb`
//!   transaction for fast rebuilds.

use std::fs::{self, File, OpenOptions as FsOpenOptions};
use std::io::{BufRead, BufReader, Read as IoRead, Seek, SeekFrom, Write as IoWrite};
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::projection::{read_stamp, reset, OnMismatch, OpenOptions, ProjectionId, ProjectionStamp};
use crate::store::{Db, Read, ReadTx, Tx, BOOKKEEPING_TABLE};
use crate::watch::WatchRange;
use crate::{Durability, Error, Result, Schema};

pub(crate) const KEY_APPLIED: &[u8] = b"applied";
pub(crate) const KEY_APPLIED_OFFSET: &[u8] = b"applied_byte_offset";
pub(crate) const KEY_IDS_PREFIX: &[u8] = b"ids/";

/// Number of events folded per `redb` write transaction during `catch_up` / `rebuild`.
const REPLAY_CHUNK_SIZE: usize = 4096;

/// Authoritative ingest metadata. Assigned by [`Runtime`] at append time.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Meta {
    pub seq: u64,
    pub ts_ms: u64,
}

/// One fsynced log line: ingest metadata plus the event body.
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
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
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
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Helper<E> {
            seq: u64,
            ts_ms: u64,
            from: u64,
            to: u64,
            #[serde(default)]
            id: Option<String>,
            #[serde(default)]
            idh: Option<String>,
            event: E,
            crc: String,
        }
        let h = Helper::deserialize(d)?;
        if h.id.is_some() != h.idh.is_some() {
            return Err(serde::de::Error::custom("id and idh must appear together"));
        }
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

/// Outcome of an idempotent append (`append_idempotent` / `append_batch_idempotent`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AppendOutcome<E> {
    /// Newly appended and committed.
    Committed(Vec<Record<E>>),
    /// Previously committed with the same idempotency key and payload hash.
    Duplicate(Vec<Record<E>>),
}

impl<E> AppendOutcome<E> {
    pub fn records(&self) -> &[Record<E>] {
        match self {
            AppendOutcome::Committed(r) | AppendOutcome::Duplicate(r) => r,
        }
    }

    pub fn into_records(self) -> Vec<Record<E>> {
        match self {
            AppendOutcome::Committed(r) | AppendOutcome::Duplicate(r) => r,
        }
    }

    pub fn is_duplicate(&self) -> bool {
        matches!(self, AppendOutcome::Duplicate(_))
    }
}

/// CRC32C over line bytes as written, rendered as 8 lowercase hex digits.
pub fn seal_crc(prefix_bytes: &[u8]) -> String {
    format!("{:08x}", crc32c::crc32c(prefix_bytes))
}

/// Render one sealed tape line: compact JSON, `seq` first, CRC suffix always
/// exactly `,"crc":"xxxxxxxx"}` (18 bytes).
pub fn render_sealed<E: Serialize>(
    seq: u64,
    ts_ms: u64,
    from: u64,
    to: u64,
    event: &E,
) -> Result<String> {
    render_sealed_with_id(seq, ts_ms, from, to, None, event)
}

/// Render one sealed tape line, optionally carrying an idempotency `(id, idh)`
/// pair (only valid on the closing line `seq == to`).
pub fn render_sealed_with_id<E: Serialize>(
    seq: u64,
    ts_ms: u64,
    from: u64,
    to: u64,
    idempotency: Option<(&str, &str)>,
    event: &E,
) -> Result<String> {
    let body = serde_json::to_string(event).map_err(|e| Error::Serialize(e.to_string()))?;
    let mut prefix = match idempotency {
        Some((id, idh)) => {
            let id_json =
                serde_json::to_string(id).map_err(|e| Error::Serialize(e.to_string()))?;
            let idh_json =
                serde_json::to_string(idh).map_err(|e| Error::Serialize(e.to_string()))?;
            format!(
                "{{\"seq\":{seq},\"ts_ms\":{ts_ms},\"from\":{from},\"to\":{to},\"id\":{id_json},\"idh\":{idh_json},\"event\":{body}"
            )
        }
        None => {
            format!("{{\"seq\":{seq},\"ts_ms\":{ts_ms},\"from\":{from},\"to\":{to},\"event\":{body}")
        }
    };
    let crc = seal_crc(prefix.as_bytes());
    prefix.push_str(",\"crc\":\"");
    prefix.push_str(&crc);
    prefix.push_str("\"}");
    Ok(prefix)
}

/// Compute a 32-byte payload hash (rendered as 64 lowercase hex digits) over a
/// batch of serialized event JSON bodies.
fn batch_payload_hash(serialized_events: &[String]) -> String {
    let mut h = Sha256::new();
    h.update(b"durable.idh.v1\0");
    h.update((serialized_events.len() as u64).to_be_bytes());
    for body in serialized_events {
        h.update((body.len() as u64).to_be_bytes());
        h.update(body.as_bytes());
    }
    let digest = h.finalize();
    let mut hex = String::with_capacity(64);
    for b in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut hex, "{b:02x}");
    }
    hex
}

/// A checked sealed line envelope.
#[derive(Debug, Clone)]
pub struct SealedEnvelope {
    pub seq: u64,
    pub ts_ms: u64,
    pub from: u64,
    pub to: u64,
    pub id: Option<String>,
    pub idh: Option<String>,
}

/// Maximum sealed line: 16 MiB.
pub const MAX_SEALED_LINE: usize = 16 * 1024 * 1024;
/// Largest batch the scan will call complete: bounds `to - from`.
pub const MAX_SEALED_BATCH: u64 = 1024;
/// CRC suffix length: `,"crc":"xxxxxxxx"}` is always exactly 18 bytes.
pub const CRC_SUFFIX_LEN: usize = 18;

/// Result of scanning a sealed tape: committed prefix ends at byte `end`,
/// holds `len` records, with highest ingest timestamp `last_ts`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Committed {
    pub end: usize,
    pub len: u64,
    pub last_ts: u64,
}

/// Where the scan detected corruption: byte `offset` and reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Corruption {
    pub offset: usize,
    pub reason: String,
}

/// Pure commit-rule scanner over an in-memory byte slice.
pub fn scan(bytes: &[u8]) -> std::result::Result<Committed, Corruption> {
    scan_reader(bytes)
}

/// Streaming commit-rule scanner: O(1) memory (bounded by [`MAX_SEALED_LINE`]).
pub fn scan_reader<R: BufRead>(mut reader: R) -> std::result::Result<Committed, Corruption> {
    let mut committed = Committed {
        end: 0,
        len: 0,
        last_ts: 0,
    };
    let mut inside: Option<(u64, u64, u64)> = None;
    let mut bad: Option<(usize, String)> = None;
    let mut offset = 0usize;
    let mut buf = Vec::new();
    loop {
        buf.clear();
        let read = read_line_bounded(&mut reader, &mut buf).map_err(|e| Corruption {
            offset,
            reason: format!("read failed: {e}"),
        })?;
        let Some(read) = read else { break };
        let start = offset;
        offset += read.consumed;
        if !read.terminated {
            break;
        }
        let chunk: &[u8] = if read.overlong {
            &[]
        } else {
            &buf[..buf.len() - 1]
        };
        if read.overlong {
            if bad.is_none() {
                bad = Some((
                    start,
                    "not a candidate line: sealed line over 16 MiB".into(),
                ));
            }
            continue;
        }
        if chunk.iter().all(|b| b.is_ascii_whitespace()) {
            continue;
        }
        if bad.is_some() {
            tail_check(chunk, start, committed.len, &bad)?;
            continue;
        }
        match classify(chunk) {
            Classified::Foreign(reason) => {
                return Err(Corruption {
                    offset: start,
                    reason,
                })
            }
            Classified::Damaged(reason) => {
                bad = Some((start, format!("not a candidate line: {reason}")));
                tail_check(chunk, start, committed.len, &bad)?;
            }
            Classified::Candidate(env, _event) => {
                let coherent = match inside {
                    None => {
                        if env.from == committed.len && env.seq == env.from {
                            inside = Some((env.from, env.to, env.seq + 1));
                            true
                        } else {
                            bad = Some((
                                start,
                                format!(
                                    "batch opens at from={} seq={} but tape holds {}",
                                    env.from, env.seq, committed.len
                                ),
                            ));
                            false
                        }
                    }
                    Some((from, to, next_seq)) => {
                        if env.from == from && env.to == to && env.seq == next_seq {
                            inside = Some((from, to, next_seq + 1));
                            true
                        } else {
                            bad = Some((
                                start,
                                format!(
                                    "batch [{from}..{to}] expects seq {next_seq} but got seq {}",
                                    env.seq
                                ),
                            ));
                            false
                        }
                    }
                };
                if !coherent {
                    tail_check(chunk, start, committed.len, &bad)?;
                    continue;
                }
                if env.seq == env.to {
                    committed = Committed {
                        end: offset,
                        len: env.to + 1,
                        last_ts: committed.last_ts.max(env.ts_ms),
                    };
                    inside = None;
                }
            }
        }
    }
    Ok(committed)
}

fn tail_check(
    chunk: &[u8],
    line_offset: usize,
    committed_len: u64,
    bad: &Option<(usize, String)>,
) -> std::result::Result<(), Corruption> {
    if chunk.iter().all(|b| b.is_ascii_whitespace()) {
        return Ok(());
    }
    match classify(chunk) {
        Classified::Candidate(env, _) => {
            if env.from != committed_len {
                let (off, reason) = bad.clone().unwrap_or((line_offset, String::new()));
                return Err(Corruption {
                    offset: off,
                    reason,
                });
            }
            Ok(())
        }
        Classified::Foreign(reason) => Err(Corruption {
            offset: line_offset,
            reason,
        }),
        Classified::Damaged(_) => {
            if is_unsealed_json(chunk) {
                return Err(Corruption {
                    offset: line_offset,
                    reason: "unsealed line: not written by the sealed writer \
                             (migrate old tapes offline before opening)"
                        .into(),
                });
            }
            Ok(())
        }
    }
}

fn is_unsealed_json(line: &[u8]) -> bool {
    let has_crc_suffix = line.len() >= CRC_SUFFIX_LEN
        && line[line.len() - CRC_SUFFIX_LEN..].starts_with(b",\"crc\":\"")
        && line.ends_with(b"\"}");
    !has_crc_suffix && serde_json::from_slice::<serde::de::IgnoredAny>(line).is_ok()
}

pub(crate) enum Classified<'a> {
    Candidate(SealedEnvelope, &'a [u8]),
    Damaged(String),
    Foreign(String),
}

pub(crate) fn classify(line: &[u8]) -> Classified<'_> {
    if line.len() > MAX_SEALED_LINE {
        return Classified::Damaged("sealed line over 16 MiB".into());
    }
    if line.len() < CRC_SUFFIX_LEN + 2 {
        return Classified::Damaged("sealed line too short for a crc".into());
    }
    let (prefix, suffix) = line.split_at(line.len() - CRC_SUFFIX_LEN);
    if !suffix.starts_with(b",\"crc\":\"") || !suffix.ends_with(b"\"}") {
        return Classified::Damaged("sealed line missing crc suffix".into());
    }
    let got = match std::str::from_utf8(&suffix[8..16]) {
        Ok(s) => s,
        Err(e) => return Classified::Damaged(e.to_string()),
    };
    if !got.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Classified::Damaged("sealed crc not hex".into());
    }
    if got != seal_crc(prefix) {
        return Classified::Damaged("sealed crc mismatch".into());
    }
    let env: SealedEnvelopeWire = match serde_json::from_slice(line) {
        Ok(env) => env,
        Err(e) => {
            return Classified::Foreign(format!(
                "crc-valid sealed line this build does not read (unknown or mistyped envelope field?): {e}"
            ))
        }
    };
    if env.id.is_some() != env.idh.is_some() {
        return Classified::Foreign(
            "crc-valid sealed line with mismatched id/idh envelope fields".into(),
        );
    }
    if env.id.is_some() && env.seq != env.to {
        return Classified::Foreign(
            "crc-valid sealed line carrying id/idh before batch closing line".into(),
        );
    }
    if env.from > env.to || env.seq < env.from || env.seq > env.to {
        return Classified::Damaged("sealed envelope seq outside batch".into());
    }
    if env.to - env.from >= MAX_SEALED_BATCH {
        return Classified::Damaged("sealed batch too large".into());
    }
    Classified::Candidate(
        SealedEnvelope {
            seq: env.seq,
            ts_ms: env.ts_ms,
            from: env.from,
            to: env.to,
            id: env.id,
            idh: env.idh,
        },
        env.event.get().as_bytes(),
    )
}

/// Parse one sealed line into its envelope and raw event bytes.
pub fn parse_sealed(line: &[u8]) -> Result<(SealedEnvelope, Vec<u8>)> {
    match classify(line) {
        Classified::Candidate(env, event) => Ok((env, event.to_vec())),
        Classified::Damaged(reason) | Classified::Foreign(reason) => {
            Err(Error::Deserialize(reason))
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SealedEnvelopeWire<'a> {
    seq: u64,
    ts_ms: u64,
    from: u64,
    to: u64,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    idh: Option<String>,
    #[serde(borrow)]
    event: &'a serde_json::value::RawValue,
    #[allow(dead_code)]
    crc: serde::de::IgnoredAny,
}

struct LineRead {
    consumed: usize,
    terminated: bool,
    overlong: bool,
}

fn read_line_bounded<R: BufRead>(
    reader: &mut R,
    buf: &mut Vec<u8>,
) -> std::io::Result<Option<LineRead>> {
    let mut consumed = 0usize;
    let mut terminated = false;
    let mut overlong = false;
    loop {
        let avail = reader.fill_buf()?;
        if avail.is_empty() {
            break;
        }
        let (take, done) = match avail.iter().position(|b| *b == b'\n') {
            Some(i) => (i + 1, true),
            None => (avail.len(), false),
        };
        if !overlong {
            if buf.len() + take > MAX_SEALED_LINE + 1 {
                overlong = true;
                buf.clear();
            } else {
                buf.extend_from_slice(&avail[..take]);
            }
        }
        reader.consume(take);
        consumed += take;
        if done {
            terminated = true;
            break;
        }
    }
    if consumed == 0 {
        return Ok(None);
    }
    Ok(Some(LineRead {
        consumed,
        terminated,
        overlong,
    }))
}

/// Render a sealed line whose event is already-valid JSON bytes, copied verbatim.
pub(crate) fn render_sealed_raw(
    seq: u64,
    ts_ms: u64,
    from: u64,
    to: u64,
    event_json: &[u8],
) -> String {
    let mut prefix =
        format!("{{\"seq\":{seq},\"ts_ms\":{ts_ms},\"from\":{from},\"to\":{to},\"event\":");
    prefix.push_str(std::str::from_utf8(event_json).expect("event JSON is UTF-8"));
    let crc = seal_crc(prefix.as_bytes());
    prefix.push_str(",\"crc\":\"");
    prefix.push_str(&crc);
    prefix.push_str("\"}");
    prefix
}

/// Fold one record into `db` in its own transaction (used by `equiv` / `rewrite`).
pub(crate) fn fold_one<E>(
    db: &Db,
    reducer: &impl Fn(&mut Tx, &E) -> Result<()>,
    meta: Meta,
    event: &E,
) -> Result<()> {
    let mut tx = db.write_with(Durability::DisableWal)?;
    tx.set_meta(meta);
    reducer(&mut tx, event)?;
    write_applied(&mut tx, meta.seq + 1, None)?;
    tx.commit()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Bookkeeping helpers inside `__durable`
// ---------------------------------------------------------------------------

pub(crate) fn read_applied(r: &impl Read) -> Result<u64> {
    match r.raw_get(BOOKKEEPING_TABLE, KEY_APPLIED)? {
        Some(bytes) => {
            if bytes.len() != 8 {
                return Err(Error::Corruption("malformed applied offset".into()));
            }
            let mut buf = [0u8; 8];
            buf.copy_from_slice(&bytes);
            Ok(u64::from_le_bytes(buf))
        }
        None => Ok(0),
    }
}

pub(crate) fn read_applied_offset(r: &impl Read) -> Result<Option<u64>> {
    match r.raw_get(BOOKKEEPING_TABLE, KEY_APPLIED_OFFSET)? {
        Some(bytes) => {
            if bytes.len() != 8 {
                return Err(Error::Corruption("malformed applied_byte_offset".into()));
            }
            let mut buf = [0u8; 8];
            buf.copy_from_slice(&bytes);
            Ok(Some(u64::from_le_bytes(buf)))
        }
        None => Ok(None),
    }
}

pub(crate) fn write_applied(tx: &mut Tx, applied: u64, byte_offset: Option<u64>) -> Result<()> {
    tx.raw_put(BOOKKEEPING_TABLE, KEY_APPLIED, &applied.to_le_bytes())?;
    if let Some(off) = byte_offset {
        tx.raw_put(BOOKKEEPING_TABLE, KEY_APPLIED_OFFSET, &off.to_le_bytes())?;
    }
    Ok(())
}

fn idempotency_key(id: &str) -> Vec<u8> {
    let mut key = Vec::with_capacity(KEY_IDS_PREFIX.len() + id.len());
    key.extend_from_slice(KEY_IDS_PREFIX);
    key.extend_from_slice(id.as_bytes());
    key
}

/// Stored idempotency entry: `(from, to, idh)`.
#[derive(Serialize, Deserialize)]
struct IdRecord {
    from: u64,
    to: u64,
    idh: String,
}

fn read_idempotency(r: &impl Read, id: &str) -> Result<Option<IdRecord>> {
    match r.raw_get(BOOKKEEPING_TABLE, &idempotency_key(id))? {
        Some(bytes) => Ok(Some(crate::decode_value(&bytes)?)),
        None => Ok(None),
    }
}

fn write_idempotency(tx: &mut Tx, id: &str, from: u64, to: u64, idh: &str) -> Result<()> {
    let rec = IdRecord {
        from,
        to,
        idh: idh.to_string(),
    };
    let bytes = crate::encode_value(&rec)?;
    tx.raw_put(BOOKKEEPING_TABLE, &idempotency_key(id), &bytes)
}

// ---------------------------------------------------------------------------
// JsonlLog + TapeFile (fault-injection seam)
// ---------------------------------------------------------------------------

/// Append-only self-sealing JSONL event log.
pub struct JsonlLog<E> {
    path: PathBuf,
    file: Box<dyn TapeFile>,
    len: u64,
    end: u64,
    poisoned: bool,
    _event: PhantomData<E>,
}

/// Handle indirection for the tape file (real [`File`] in production, failing
/// double in tests).
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

#[cfg(feature = "fault-injection")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailMode {
    WriteEnospc,
    WritePartialEnospc(usize),
    WritePartialEnospcAndTruncateEperm(usize),
    SyncEio,
    SyncEioOnce,
    TruncateEperm,
    SyncEioAndTruncateEperm,
    ShrunkTo(u64),
}

#[cfg(feature = "fault-injection")]
#[derive(Debug)]
pub struct FailingFile {
    disk: Arc<Mutex<Vec<u8>>>,
    mode: Option<FailMode>,
    syncs_failed: std::cell::Cell<usize>,
}

#[cfg(feature = "fault-injection")]
pub type FakeDisk = Arc<Mutex<Vec<u8>>>;

#[cfg(feature = "fault-injection")]
impl FailingFile {
    pub fn over(bytes: Vec<u8>, mode: Option<FailMode>) -> (Self, FakeDisk) {
        let disk = Arc::new(Mutex::new(bytes));
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
        let mut file = FsOpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(&path)
            .map_err(|e| Error::Log(e.to_string()))?;
        let len = count_jsonl_lines(&mut file)?;
        let end = file
            .metadata()
            .map_err(|e| Error::Log(e.to_string()))?
            .len();
        Ok(Self {
            path,
            file: Box::new(file),
            len,
            end,
            poisoned: false,
            _event: PhantomData,
        })
    }

    pub fn open_sealed(path: impl AsRef<Path>) -> Result<(Self, Committed)> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| Error::Log(e.to_string()))?;
        }
        if !path.exists() {
            let log = Self::open(&path)?;
            return Ok((
                log,
                Committed {
                    end: 0,
                    len: 0,
                    last_ts: 0,
                },
            ));
        }
        let found = Self::scan_file(&path)?;
        Self::seal_to(&path, &found)?;
        let mut log = Self::open(&path)?;
        log.len = found.len;
        log.end = found.end as u64;
        Ok((log, found))
    }

    /// Streaming read-only scan of the tape file (O(1) memory, file untouched).
    pub fn scan_file(path: &Path) -> Result<Committed> {
        let file = File::open(path).map_err(|e| Error::Log(e.to_string()))?;
        let reader = BufReader::new(file);
        scan_reader(reader).map_err(|c| {
            Error::Corruption(format!("tape corrupt at byte {}: {}", c.offset, c.reason))
        })
    }

    /// Truncate uncommitted tail residue past `found.end`, then sync unconditionally.
    pub fn seal_to(path: &Path, found: &Committed) -> Result<()> {
        let f = FsOpenOptions::new()
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
        f.sync_data().map_err(|e| Error::Log(e.to_string()))?;
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn len(&self) -> Result<u64> {
        Ok(self.len)
    }

    pub fn committed_end(&self) -> u64 {
        self.end
    }

    pub fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    #[cfg(feature = "fault-injection")]
    pub fn inject_failing_file(&mut self, fail: FailingFile) {
        self.file = Box::new(fail);
    }

    pub fn is_empty(&self) -> Result<bool> {
        Ok(self.len == 0)
    }

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

    fn append_raw_batch(&mut self, batch_bytes: &[u8], count: u64) -> Result<u64> {
        let start = self.len;
        if count == 0 {
            return Ok(start);
        }
        if self.poisoned {
            return Err(Error::Log(
                "tape writer poisoned by an earlier I/O failure; reopen the tape".into(),
            ));
        }
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
        if let Err(e) = self.file.write_all(batch_bytes) {
            self.rollback()?;
            return Err(Error::Log(format!("not committed: {e}")));
        }
        if let Err(e) = self.file.sync_data() {
            let rolled = self.rollback().is_ok();
            self.poisoned = true;
            return Err(Error::Log(if rolled {
                format!("not committed (writer poisoned): {e}")
            } else {
                format!("commit outcome unknown (writer poisoned): {e}")
            }));
        }
        self.end += batch_bytes.len() as u64;
        self.len += count;
        Ok(start)
    }
}

impl<E: Serialize + DeserializeOwned> JsonlLog<Record<E>> {
    /// Append sealed records as consecutive lines and fsync once.
    pub fn append_all(&mut self, events: &[Record<E>]) -> Result<u64> {
        self.append_all_with_id(events, None)
    }

    fn append_all_with_id(
        &mut self,
        events: &[Record<E>],
        idempotency: Option<(&str, &str)>,
    ) -> Result<u64> {
        if events.is_empty() {
            return Ok(self.len);
        }
        let mut batch = String::with_capacity(events.len() * 256);
        let last = events.len() - 1;
        for (i, rec) in events.iter().enumerate() {
            let id_pair = if i == last { idempotency } else { None };
            let line =
                render_sealed_with_id(rec.seq, rec.ts_ms, rec.from, rec.to, id_pair, &rec.event)?;
            batch.push_str(&line);
            batch.push('\n');
        }
        self.append_raw_batch(batch.as_bytes(), events.len() as u64)
    }

    /// Stream committed records from `start_seq`, optionally seeking directly
    /// to `start_offset` (when known to align with `start_seq`). Passes
    /// `(record, env_id_pair, end_byte_offset_after_line)` to `f`.
    pub(crate) fn for_each_from(
        &self,
        start_seq: u64,
        start_offset: Option<u64>,
        mut f: impl FnMut(Record<E>, Option<(String, String)>, u64) -> Result<()>,
    ) -> Result<()> {
        if !self.path.exists() || self.end == 0 {
            return Ok(());
        }
        let mut file = File::open(&self.path).map_err(|e| Error::Log(e.to_string()))?;
        let seek_to = match start_offset {
            Some(off) if off <= self.end => off,
            _ => 0,
        };
        if seek_to > 0 {
            file.seek(SeekFrom::Start(seek_to))
                .map_err(|e| Error::Log(e.to_string()))?;
        }
        let mut reader = BufReader::new(file.take(self.end - seek_to));
        let mut offset = seek_to;
        let mut buf = Vec::new();
        loop {
            buf.clear();
            let read = read_line_bounded(&mut reader, &mut buf)
                .map_err(|e| Error::Log(e.to_string()))?;
            let Some(read) = read else { break };
            offset += read.consumed as u64;
            if !read.terminated || read.overlong {
                return Err(Error::Deserialize("unexpected torn or overlong line in committed prefix".into()));
            }
            let chunk = &buf[..buf.len() - 1];
            if chunk.is_empty() || chunk.iter().all(|b| b.is_ascii_whitespace()) {
                continue;
            }
            let (env, event_bytes) = parse_sealed(chunk)?;
            if env.seq < start_seq {
                continue;
            }
            let event: E = serde_json::from_slice(&event_bytes)
                .map_err(|e| Error::Deserialize(e.to_string()))?;
            let crc = String::from_utf8_lossy(
                &chunk[chunk.len() - CRC_SUFFIX_LEN + 8..chunk.len() - 2],
            )
            .into_owned();
            let id_pair = match (env.id, env.idh) {
                (Some(id), Some(idh)) => Some((id, idh)),
                _ => None,
            };
            let rec = Record {
                seq: env.seq,
                ts_ms: env.ts_ms,
                from: env.from,
                to: env.to,
                event,
                crc,
            };
            f(rec, id_pair, offset)?;
        }
        Ok(())
    }

    /// Read committed records from `start` (inclusive) to the end, streaming
    /// from disk.
    pub fn read_from(&self, start: u64) -> Result<Vec<(u64, Record<E>)>> {
        let mut out = Vec::new();
        self.for_each_from(start, None, |rec, _, _| {
            let seq = rec.seq;
            out.push((seq, rec));
            Ok(())
        })?;
        Ok(out)
    }

    pub fn read_all(&self) -> Result<Vec<(u64, Record<E>)>> {
        self.read_from(0)
    }
}

// ---------------------------------------------------------------------------
// Free-space floor check (Unix statvfs)
// ---------------------------------------------------------------------------

#[cfg(unix)]
fn check_free_space(path: &Path, min_free_bytes: u64, needed: u64) -> Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let target = if path.exists() {
        path
    } else {
        path.parent().unwrap_or(Path::new("."))
    };
    let c_path =
        CString::new(target.as_os_str().as_bytes()).map_err(|e| Error::Log(e.to_string()))?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) };
    if rc != 0 {
        return Err(Error::Log(format!(
            "statvfs failed: {}",
            std::io::Error::last_os_error()
        )));
    }
    let avail = (stat.f_bavail as u64).saturating_mul(stat.f_frsize as u64);
    if avail < min_free_bytes.saturating_add(needed) {
        return Err(Error::NoSpace {
            available: avail,
            required: min_free_bytes.saturating_add(needed),
        });
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_free_space(_path: &Path, _min_free_bytes: u64, _needed: u64) -> Result<()> {
    Ok(())
}

// ---------------------------------------------------------------------------
// Runtime<E>
// ---------------------------------------------------------------------------

struct Inner<E> {
    log: JsonlLog<Record<E>>,
    applied: u64,
    applied_offset: Option<u64>,
    last_ts_ms: u64,
    /// Set if a replay-time reducer error occurs on an already-committed tape
    /// event. Refuses all appends until reopened with a fixed reducer.
    halted: Option<(u64, String)>,
}

/// Event-sourced runtime: self-sealing JSONL tape + typed `redb` projection.
pub struct Runtime<E> {
    inner: Mutex<Inner<E>>,
    applied: AtomicU64,
    pub(crate) db: Db,
    reducer: crate::projection::Reducer<E>,
    min_free_bytes: Option<u64>,
    on_commit: Mutex<Option<Arc<dyn Fn(u64, &[WatchRange]) + Send + Sync>>>,
    last_touched: Mutex<Vec<WatchRange>>,
    pub(crate) identity: Option<ProjectionStamp>,
    #[allow(dead_code)]
    tape_lock: Option<File>,
}

impl<E: Serialize + DeserializeOwned + 'static> Runtime<E> {
    /// Open (or create) a runtime with [`ProjectionId::new(0)`], exclusive
    /// tape locking, schema fingerprinting, and automatic rebuild on schema change.
    pub fn open<S: Schema>(
        db_path: impl AsRef<Path>,
        log_path: impl AsRef<Path>,
        reducer: impl Fn(&mut Tx, &E) -> Result<()> + Send + Sync + 'static,
    ) -> Result<Self> {
        Self::open_with(
            db_path,
            log_path,
            OpenOptions::new::<S>(ProjectionId::new(0), reducer),
        )
    }

    /// Open with explicit [`OpenOptions`] (custom [`ProjectionId`],
    /// [`OnMismatch`], and optional free-space floor).
    pub fn open_with(
        db_path: impl AsRef<Path>,
        log_path: impl AsRef<Path>,
        options: OpenOptions<E>,
    ) -> Result<Self> {
        Self::open_impl(db_path.as_ref(), log_path.as_ref(), options, true)
    }

    /// Open without locking the tape or checking/stamping projection identity.
    /// Useful only for read-only replays or legacy migration tooling.
    pub fn open_unmanaged(
        db_path: impl AsRef<Path>,
        log_path: impl AsRef<Path>,
        reducer: impl Fn(&mut Tx, &E) -> Result<()> + Send + Sync + 'static,
    ) -> Result<Self> {
        Self::open_impl(
            db_path.as_ref(),
            log_path.as_ref(),
            OpenOptions {
                schema_hash: 0,
                projection: ProjectionId::new(0),
                on_mismatch: OnMismatch::Refuse,
                min_free_bytes: None,
                reducer: Arc::new(reducer),
            },
            false,
        )
    }

    fn open_impl(
        db_path: &Path,
        log_path: &Path,
        options: OpenOptions<E>,
        managed: bool,
    ) -> Result<Self> {
        if let Some(parent) = db_path.parent() {
            fs::create_dir_all(parent).map_err(|e| Error::Log(e.to_string()))?;
        }
        if let Some(parent) = log_path.parent() {
            fs::create_dir_all(parent).map_err(|e| Error::Log(e.to_string()))?;
        }
        let tape_lock = if managed {
            Some(crate::projection::lock_tape(log_path)?)
        } else {
            None
        };
        // Scan read-only BEFORE opening/sealing so disagreement leaves tape untouched.
        let found = if log_path.exists() {
            JsonlLog::<Record<E>>::scan_file(log_path)?
        } else {
            Committed {
                end: 0,
                len: 0,
                last_ts: 0,
            }
        };
        let db = Db::open(db_path)?;
        let (stored, applied, applied_offset) = {
            let r = db.read()?;
            (read_stamp(&r)?, read_applied(&r)?, read_applied_offset(&r)?)
        };
        let (identity, rebuild) = if !managed {
            if let Some(s) = stored {
                if s.reducer != 0 {
                    return Err(Error::ProjectionMismatch {
                        stored: Some(s),
                        wanted: ProjectionStamp::from_parts(0, options.schema_hash),
                    });
                }
            }
            (stored, false)
        } else {
            let wanted = ProjectionStamp::from_parts(options.projection.reducer, options.schema_hash);
            if stored == Some(wanted) {
                (Some(wanted), false)
            } else {
                let fresh = stored.is_none() && applied == 0;
                if options.on_mismatch == OnMismatch::Refuse && !fresh {
                    return Err(Error::ProjectionMismatch { stored, wanted });
                }
                (Some(wanted), true)
            }
        };
        let (applied, applied_offset) = if rebuild {
            reset(&db, identity)?;
            (0, Some(0))
        } else {
            if applied > found.len {
                return Err(Error::Corruption(format!(
                    "projection and tape disagree (applied={applied}, tape has {})",
                    found.len
                )));
            }
            (applied, applied_offset)
        };
        if log_path.exists() {
            JsonlLog::<Record<E>>::seal_to(log_path, &found)?;
        }
        let mut log = JsonlLog::open(log_path)?;
        log.len = found.len;
        log.end = found.end as u64;
        let last_ts_ms = found.last_ts;
        let rt = Self {
            inner: Mutex::new(Inner {
                log,
                applied,
                applied_offset,
                last_ts_ms,
                halted: None,
            }),
            applied: AtomicU64::new(applied),
            db,
            reducer: options.reducer,
            min_free_bytes: options.min_free_bytes,
            on_commit: Mutex::new(None),
            last_touched: Mutex::new(Vec::new()),
            identity,
            tape_lock,
        };
        rt.catch_up()?;
        Ok(rt)
    }

    /// Open a snapshot-consistent [`ReadTx`] over the projection.
    ///
    /// Never blocks on an in-flight append/fsync and never observes partial
    /// batches.
    pub fn read(&self) -> Result<ReadTx> {
        self.db.read()
    }

    /// Direct handle to the underlying `redb` [`Db`].
    pub fn db(&self) -> &Db {
        &self.db
    }

    /// Register a commit listener fired after every committed batch with the
    /// closing `seq` and the touched [`WatchRange`]s.
    pub fn on_commit(&self, f: impl Fn(u64, &[WatchRange]) + Send + Sync + 'static) {
        *self.on_commit.lock().expect("on_commit lock") = Some(Arc::new(f));
    }

    /// Remove the commit listener.
    pub fn clear_on_commit(&self) {
        *self.on_commit.lock().expect("on_commit lock") = None;
    }

    /// Touched ranges of the most recent committed batch.
    pub fn last_touched(&self) -> Vec<WatchRange> {
        self.last_touched.lock().expect("last_touched lock").clone()
    }

    /// Path of the canonical JSONL tape.
    pub fn log_path(&self) -> Result<PathBuf> {
        let inner = self
            .inner
            .lock()
            .map_err(|_| Error::Log("runtime lock poisoned".into()))?;
        Ok(inner.log.path().to_path_buf())
    }

    /// Number of committed records on the tape.
    pub fn log_len(&self) -> Result<u64> {
        let inner = self
            .inner
            .lock()
            .map_err(|_| Error::Log("runtime lock poisoned".into()))?;
        inner.log.len()
    }

    /// Published committed byte offset of the tape (safe cutoff for live backups).
    pub fn committed_end(&self) -> Result<u64> {
        let inner = self
            .inner
            .lock()
            .map_err(|_| Error::Log("runtime lock poisoned".into()))?;
        Ok(inner.log.committed_end())
    }

    /// Copy the committed prefix `[0..committed_end]` of the live tape to `dst`
    /// atomically (temp file beside `dst`, fsynced, then renamed). Returns the
    /// number of bytes copied.
    pub fn backup_tape(&self, dst: impl AsRef<Path>) -> Result<u64> {
        let dst = dst.as_ref();
        let (src_path, end) = {
            let inner = self
                .inner
                .lock()
                .map_err(|_| Error::Log("runtime lock poisoned".into()))?;
            (inner.log.path().to_path_buf(), inner.log.committed_end())
        };
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent).map_err(|e| Error::Log(e.to_string()))?;
        }
        let tmp = dst.with_extension(format!("backup-tmp-{}", crate::equiv::unique_suffix()));
        let copy_res = (|| -> Result<u64> {
            let mut out = FsOpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&tmp)
                .map_err(|e| Error::Log(e.to_string()))?;
            if src_path.exists() && end > 0 {
                let src = File::open(&src_path).map_err(|e| Error::Log(e.to_string()))?;
                let mut limited = src.take(end);
                std::io::copy(&mut limited, &mut out).map_err(|e| Error::Log(e.to_string()))?;
            }
            out.sync_data().map_err(|e| Error::Log(e.to_string()))?;
            drop(out);
            fs::rename(&tmp, dst).map_err(|e| Error::Log(e.to_string()))?;
            Ok(end)
        })();
        if copy_res.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        copy_res
    }

    /// Number of events applied to the projection.
    pub fn applied(&self) -> Result<u64> {
        Ok(self.applied.load(Ordering::Acquire))
    }

    /// If replay halted on a reducer error, returns `Some((seq, reason))`.
    pub fn halted(&self) -> Result<Option<(u64, String)>> {
        let inner = self
            .inner
            .lock()
            .map_err(|_| Error::Log("runtime lock poisoned".into()))?;
        Ok(inner.halted.clone())
    }

    /// Committed records on the tape from `start` (inclusive) to the end.
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

    /// Append a single event: validate via reducer first (rejections leave the
    /// tape untouched), fsync the tape, then commit the projection atomically.
    pub fn append(&self, event: E) -> Result<Record<E>> {
        let mut recs = self.append_batch(vec![event])?;
        recs.pop()
            .ok_or_else(|| Error::Log("append_batch returned empty".into()))
    }

    /// Append a batch of events as one group commit:
    /// 1. Reduce all events sequentially inside one `redb::WriteTransaction`
    ///    (read-your-writes across the batch). If any event fails, the
    ///    transaction aborts and the tape is untouched.
    /// 2. Render + check free-space floor + write + single `sync_data` to tape.
    /// 3. Commit the `WriteTransaction` atomically (whole batch visible together).
    pub fn append_batch(&self, events: Vec<E>) -> Result<Vec<Record<E>>> {
        Ok(self.append_batch_impl(events, None)?.into_records())
    }

    /// Idempotent single-event append keyed by `id`.
    ///
    /// - First call commits and returns [`AppendOutcome::Committed`].
    /// - Retry with the same `id` and equal serialized event returns
    ///   [`AppendOutcome::Duplicate`] without appending a second copy.
    /// - Retry with the same `id` and a different payload returns
    ///   [`Error::Conflict`].
    pub fn append_idempotent(&self, id: &str, event: E) -> Result<AppendOutcome<E>> {
        self.append_batch_impl(vec![event], Some(id))
    }

    /// Idempotent batch append keyed by `id`.
    pub fn append_batch_idempotent(
        &self,
        id: &str,
        events: Vec<E>,
    ) -> Result<AppendOutcome<E>> {
        self.append_batch_impl(events, Some(id))
    }

    fn append_batch_impl(
        &self,
        events: Vec<E>,
        idempotency_id: Option<&str>,
    ) -> Result<AppendOutcome<E>> {
        if events.is_empty() {
            return Ok(AppendOutcome::Committed(Vec::new()));
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
        if let Some((seq, ref reason)) = inner.halted {
            return Err(Error::Halted {
                seq,
                reason: reason.clone(),
            });
        }
        if inner.log.is_poisoned() {
            return Err(Error::Log(
                "tape writer poisoned by an earlier I/O failure; reopen the tape".into(),
            ));
        }
        // Catch up before answering idempotency lookups or reducing.
        self.catch_up_locked(&mut inner)?;

        // Pre-serialize bodies once so idh and tape lines see identical bytes.
        let serialized_bodies: Vec<String> = events
            .iter()
            .map(|e| serde_json::to_string(e).map_err(|err| Error::Serialize(err.to_string())))
            .collect::<Result<_>>()?;

        let idh_opt: Option<String> = idempotency_id.map(|_| batch_payload_hash(&serialized_bodies));

        if let (Some(id), Some(ref idh)) = (idempotency_id, &idh_opt) {
            let r = self.db.read()?;
            if let Some(existing) = read_idempotency(&r, id)? {
                if &existing.idh != idh {
                    return Err(Error::Conflict {
                        id: id.to_string(),
                    });
                }
                let mut dup = Vec::new();
                inner.log.for_each_from(existing.from, None, |rec, _, _| {
                    if rec.seq >= existing.from && rec.seq <= existing.to {
                        dup.push(rec);
                    }
                    Ok(())
                })?;
                return Ok(AppendOutcome::Duplicate(dup));
            }
        }

        let from = inner.log.len()?;
        let to = from + events.len() as u64 - 1;
        let mut seq = from;
        let mut ts_ms = inner.last_ts_ms;
        let mut records = Vec::with_capacity(events.len());
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

        // Step 1: Reduce into an uncommitted `redb::WriteTransaction` FIRST.
        // Any validation failure aborts `tx` cleanly — tape never touched!
        let mut tx = self.db.write_with(Durability::DisableWal)?;
        for rec in &records {
            tx.set_meta(rec.meta());
            if let Err(e) = (self.reducer)(&mut tx, &rec.event) {
                let _ = tx.abort();
                return Err(e);
            }
        }

        // Step 2: Render sealed lines in memory.
        let mut rendered = String::with_capacity(records.len() * 256);
        let last_idx = records.len() - 1;
        for (i, (rec, body)) in records.iter_mut().zip(serialized_bodies.iter()).enumerate() {
            let mut prefix = match (i == last_idx, idempotency_id, &idh_opt) {
                (true, Some(id), Some(idh)) => {
                    let id_json =
                        serde_json::to_string(id).map_err(|e| Error::Serialize(e.to_string()))?;
                    let idh_json =
                        serde_json::to_string(idh).map_err(|e| Error::Serialize(e.to_string()))?;
                    format!(
                        "{{\"seq\":{},\"ts_ms\":{},\"from\":{},\"to\":{},\"id\":{id_json},\"idh\":{idh_json},\"event\":{body}",
                        rec.seq, rec.ts_ms, rec.from, rec.to
                    )
                }
                _ => format!(
                    "{{\"seq\":{},\"ts_ms\":{},\"from\":{},\"to\":{},\"event\":{body}",
                    rec.seq, rec.ts_ms, rec.from, rec.to
                ),
            };
            let crc = seal_crc(prefix.as_bytes());
            prefix.push_str(",\"crc\":\"");
            prefix.push_str(&crc);
            prefix.push_str("\"}");
            rec.crc = crc;
            rendered.push_str(&prefix);
            rendered.push('\n');
        }

        // Step 3: Free-space floor check before touching disk.
        if let Some(floor) = self.min_free_bytes {
            if let Err(e) = check_free_space(inner.log.path(), floor, rendered.len() as u64) {
                let _ = tx.abort();
                return Err(e);
            }
        }

        // Step 4: Write + fsync to tape.
        if let Err(e) = inner
            .log
            .append_raw_batch(rendered.as_bytes(), records.len() as u64)
        {
            let _ = tx.abort();
            return Err(e);
        }
        inner.last_ts_ms = ts_ms;
        let new_end = inner.log.committed_end();

        // Step 5: Stamp applied offset + idempotency entry and commit atomically.
        crate::fault::hit("projection.apply")?;
        write_applied(&mut tx, to + 1, Some(new_end))?;
        if let (Some(id), Some(idh)) = (idempotency_id, &idh_opt) {
            write_idempotency(&mut tx, id, from, to, idh)?;
        }
        let touched = tx.commit()?;
        *self.last_touched.lock().expect("last_touched lock") = touched.clone();
        inner.applied = to + 1;
        inner.applied_offset = Some(new_end);
        self.applied.store(inner.applied, Ordering::Release);

        // Step 6: Notify watch listeners while still holding the ingest lock.
        let listener = self.on_commit.lock().expect("on_commit lock").clone();
        if let Some(f) = listener {
            f(to, &touched);
        }
        Ok(AppendOutcome::Committed(records))
    }

    /// Replay any committed tape events the projection has not yet applied.
    pub fn catch_up(&self) -> Result<u64> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| Error::Log("runtime lock poisoned".into()))?;
        self.catch_up_locked(&mut inner)
    }

    fn catch_up_locked(&self, inner: &mut Inner<E>) -> Result<u64> {
        if let Some((seq, ref reason)) = inner.halted {
            return Err(Error::Halted {
                seq,
                reason: reason.clone(),
            });
        }
        let log_len = inner.log.len()?;
        if inner.applied >= log_len {
            return Ok(0);
        }
        let start_seq = inner.applied;
        let start_offset = if start_seq == 0 {
            Some(0)
        } else {
            inner.applied_offset
        };

        let mut chunk: Vec<(Record<E>, Option<(String, String)>, u64)> =
            Vec::with_capacity(REPLAY_CHUNK_SIZE);
        let mut total_applied = 0u64;

        let mut flush_chunk =
            |chunk: &mut Vec<(Record<E>, Option<(String, String)>, u64)>,
             inner_applied: &mut u64,
             inner_offset: &mut Option<u64>,
             inner_last_ts: &mut u64,
             inner_halted: &mut Option<(u64, String)>|
             -> Result<()> {
                if chunk.is_empty() {
                    return Ok(());
                }
                // Try applying the whole chunk in one redb WriteTransaction.
                let mut tx = self.db.write_with(Durability::DisableWal)?;
                let mut failed_at: Option<(usize, Error)> = None;
                for (idx, (rec, id_pair, _off)) in chunk.iter().enumerate() {
                    if let Err(e) = crate::fault::hit("projection.apply") {
                        let _ = tx.abort();
                        return Err(e);
                    }
                    tx.set_meta(rec.meta());
                    if let Err(e) = (self.reducer)(&mut tx, &rec.event) {
                        failed_at = Some((idx, e));
                        break;
                    }
                    if let Some((id, idh)) = id_pair {
                        write_idempotency(&mut tx, id, rec.from, rec.to, idh)?;
                    }
                }
                if let Some((bad_idx, err)) = failed_at {
                    let _ = tx.abort();
                    // Commit the prefix before `bad_idx` so `applied` stops right at the failing seq.
                    if bad_idx > 0 {
                        let mut prefix_tx = self.db.write_with(Durability::DisableWal)?;
                        for (rec, id_pair, _) in &chunk[..bad_idx] {
                            prefix_tx.set_meta(rec.meta());
                            (self.reducer)(&mut prefix_tx, &rec.event)?;
                            if let Some((id, idh)) = id_pair {
                                write_idempotency(&mut prefix_tx, id, rec.from, rec.to, idh)?;
                            }
                            *inner_last_ts = (*inner_last_ts).max(rec.ts_ms);
                        }
                        let (last_ok, _, last_off) = &chunk[bad_idx - 1];
                        write_applied(&mut prefix_tx, last_ok.seq + 1, Some(*last_off))?;
                        let touched = prefix_tx.commit()?;
                        *self.last_touched.lock().expect("last_touched lock") = touched;
                        *inner_applied = last_ok.seq + 1;
                        *inner_offset = Some(*last_off);
                        self.applied.store(*inner_applied, Ordering::Release);
                    }
                    let bad_seq = chunk[bad_idx].0.seq;
                    let reason = err.to_string();
                    *inner_halted = Some((bad_seq, reason.clone()));
                    return Err(Error::Halted {
                        seq: bad_seq,
                        reason,
                    });
                }
                for (rec, _, _) in chunk.iter() {
                    *inner_last_ts = (*inner_last_ts).max(rec.ts_ms);
                }
                let (last_rec, _, last_off) = chunk.last().unwrap();
                let new_applied = last_rec.seq + 1;
                let last_seq = last_rec.seq;
                let new_off = *last_off;
                write_applied(&mut tx, new_applied, Some(new_off))?;
                let touched = tx.commit()?;
                *self.last_touched.lock().expect("last_touched lock") = touched.clone();
                *inner_applied = new_applied;
                *inner_offset = Some(new_off);
                self.applied.store(new_applied, Ordering::Release);
                total_applied += chunk.len() as u64;
                let listener = self.on_commit.lock().expect("on_commit lock").clone();
                if let Some(f) = listener {
                    f(last_seq, &touched);
                }
                chunk.clear();
                Ok(())
            };

        let mut cur_applied = inner.applied;
        let mut cur_offset = inner.applied_offset;
        let mut cur_last_ts = inner.last_ts_ms;
        let mut cur_halted = inner.halted.clone();

        let stream_res = inner
            .log
            .for_each_from(start_seq, start_offset, |rec, id_pair, end_off| {
                chunk.push((rec, id_pair, end_off));
                if chunk.len() >= REPLAY_CHUNK_SIZE {
                    flush_chunk(
                        &mut chunk,
                        &mut cur_applied,
                        &mut cur_offset,
                        &mut cur_last_ts,
                        &mut cur_halted,
                    )?;
                }
                Ok(())
            });
        let final_res = match stream_res {
            Ok(()) => flush_chunk(
                &mut chunk,
                &mut cur_applied,
                &mut cur_offset,
                &mut cur_last_ts,
                &mut cur_halted,
            ),
            Err(e) => Err(e),
        };
        inner.applied = cur_applied;
        inner.applied_offset = cur_offset;
        inner.last_ts_ms = cur_last_ts;
        inner.halted = cur_halted;
        final_res?;
        Ok(total_applied)
    }

    /// Wipe the projection (O(tables)) and rebuild it from event 0 in chunks.
    pub fn rebuild(&self) -> Result<()> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| Error::Log("runtime lock poisoned".into()))?;
        reset(&self.db, self.identity)?;
        inner.applied = 0;
        inner.applied_offset = Some(0);
        inner.halted = None;
        self.applied.store(0, Ordering::Release);
        self.catch_up_locked(&mut inner)?;
        Ok(())
    }

    /// Property check: the live projection equals a fresh in-memory replay from
    /// zero. Uses `Db::open_memory()` (in-memory `redb`), touching no disk.
    pub fn verify(&self) -> Result<()> {
        let inner = self
            .inner
            .lock()
            .map_err(|_| Error::Log("runtime lock poisoned".into()))?;
        let mem_db = Db::open_memory()?;
        let mut tx = mem_db.write_with(Durability::DisableWal)?;
        let mut last_seq = None;
        let mut last_off = 0u64;
        inner.log.for_each_from(0, Some(0), |rec, id_pair, off| {
            tx.set_meta(rec.meta());
            (self.reducer)(&mut tx, &rec.event)?;
            if let Some((id, idh)) = id_pair {
                write_idempotency(&mut tx, &id, rec.from, rec.to, &idh)?;
            }
            last_seq = Some(rec.seq);
            last_off = off;
            Ok(())
        })?;
        if let Some(seq) = last_seq {
            write_applied(&mut tx, seq + 1, Some(last_off))?;
        }
        tx.commit()?;

        let left = crate::projection::without_stamp(self.db.dump()?);
        let right = crate::projection::without_stamp(mem_db.dump()?);
        drop(inner);
        if left != right {
            return Err(Error::Corruption(format!(
                "verify failed: incremental ({} keys) != replay ({} keys)",
                left.len(),
                right.len()
            )));
        }
        Ok(())
    }
}

fn next_ts(last: u64) -> u64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    now.max(last.saturating_add(1))
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
