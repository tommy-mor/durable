//! Offline tape rewriting: fold the old tape with the old rule, emit the new
//! tape, prove nothing else changed.
//!
//! `rewrite` reads a committed tape, folds every record with the OLD reducer
//! into a scratch projection, and lets the caller decide what each record
//! becomes in the new tape. The caller's `map` sees the state BEFORE the
//! record is reduced, so a decision that depends on the old state (who was
//! waiting, what the standings were) can be computed from exactly the state
//! the old rule saw, and recorded as data.
//!
//! Guarantees, each covered by the test suite:
//!
//! * the source is opened read-only, shared-locked, and never written;
//! * output is written to a temp file beside `dst`, fsynced, re-scanned, and
//!   required to end exactly at EOF with a valid chain before it is published;
//! * publication is `hard_link(tmp, dst)`: atomic and no-clobber (an existing
//!   `dst` makes it fail, it is never overwritten), then the temp is removed;
//! * any error before publication removes the temp and leaves `dst` absent
//!   and `src` byte-identical;
//! * an untouched record is copied byte-for-byte: in [`Mode::SameSeq`] every
//!   line that is not replaced is copied whole, CRC included; in
//!   [`Mode::Renumber`] a line is copied whole when its seq/from/to do not
//!   change, and re-sealed around the same event bytes otherwise.
//!
//! Refused: a source with bytes past its committed end (a torn tail). The
//! writer seals such a tape on open, so an operator's tape has none; a tape
//! that has one is not in a state an offline rewrite should judge.
//!
//! Not copied: blank lines (legacy padding that the scan skips). Output is
//! only as durable as the filesystem's hard-link and directory fsync
//! behaviour; on a filesystem without hard links the rewrite fails cleanly.

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write as IoWrite};
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::fault;
use crate::projection::{lock_tape_shared, write_stamp, ProjectionStamp};
use crate::runtime::{
    fold_one, parse_sealed, render_sealed, render_sealed_raw, scan_reader, Committed, Record,
    SealedEnvelope, CRC_SUFFIX_LEN,
};
use crate::store::{Db, Tx};
use crate::{Durability, Error, Result, Schema};

/// How sequence numbers look in the output tape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Output seqs, timestamps and batch bounds equal the input's. Records
    /// may be replaced 1:1 and kept; they may not be dropped.
    SameSeq,
    /// Output seqs are renumbered densely from 0 over the kept records.
    /// Batch bounds are recomputed over the kept records of each source batch
    /// (a source batch with nothing kept disappears). Timestamps are kept.
    Renumber,
}

/// What `map` decides for one record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Emit<N> {
    /// Carry the record's bytes over unchanged.
    Keep,
    /// Write this event in the record's place (same seq and ts).
    Replace(N),
    /// Remove the record. Only valid in [`Mode::Renumber`].
    Drop,
}

/// One record as the old fold sees it, plus the state before it is folded.
pub struct Step<'a, E> {
    pub rec: &'a Record<E>,
    /// Scratch projection after every earlier record, before this one.
    pub before: &'a Db,
}

/// Counts for one rewrite. Plain data; render it with [`RewriteReport::to_text`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RewriteReport {
    /// Committed records read from the source.
    pub read: u64,
    /// Records written to the output (or that would have been, in a dry run).
    pub written: u64,
    /// Records whose event `map` replaced.
    pub replaced: u64,
    /// Records `map` dropped (Renumber only).
    pub dropped: u64,
    /// Output batches written.
    pub batches_written: u64,
    /// Bytes of the source's committed prefix.
    pub src_committed_bytes: u64,
    /// Bytes of the output tape (zero in a dry run).
    pub dst_bytes: u64,
    /// Whether the directory containing `dst` was fsynced after publication.
    /// `false` means the rename is durable only as far as the filesystem
    /// flushes it; the content itself was fsynced before publication.
    pub dst_dir_synced: bool,
    /// True for [`dry_run`]: nothing was written anywhere.
    pub dry_run: bool,
}

impl RewriteReport {
    /// A plain-text report, one `key: value` per line, stable for grep.
    pub fn to_text(&self) -> String {
        format!(
            "mode: {}\nread: {}\nwritten: {}\nreplaced: {}\ndropped: {}\nbatches_written: {}\n\
             src_committed_bytes: {}\ndst_bytes: {}\ndst_dir_synced: {}\n",
            if self.dry_run { "dry-run" } else { "rewrite" },
            self.read,
            self.written,
            self.replaced,
            self.dropped,
            self.batches_written,
            self.src_committed_bytes,
            self.dst_bytes,
            self.dst_dir_synced,
        )
    }
}

/// Rewrite `src` into a new tape at `dst`.
///
/// `schema` is the projection shape the old reducer writes; the scratch
/// projection is stamped with it (see [`crate::fold_dump`]). `old_reducer` is
/// the rule that built the history. `map` is called once per committed record,
/// in order, before that record is folded.
///
/// Refuses (writing nothing, touching nothing): `dst` already exists; `src`
/// is missing, corrupt, or exclusively locked by a live writer; `map` returns
/// `Drop` in `SameSeq`; an event does not decode as `E`; `map` or the old
/// reducer errors.
pub fn rewrite<S, E, N>(
    src: &Path,
    dst: &Path,
    old_reducer: impl Fn(&mut Tx, &E) -> Result<()>,
    mode: Mode,
    map: impl FnMut(Step<'_, E>) -> Result<Emit<N>>,
) -> Result<RewriteReport>
where
    S: Schema,
    E: DeserializeOwned,
    N: Serialize,
{
    let mut out = Output::create(dst)?;
    let result = run::<S, E, N>(src, Some(&mut out), &old_reducer, mode, map);
    match result {
        Ok(mut report) => {
            // Commit point: publish. Nothing after this may fail the call.
            out.publish()?;
            report.dst_dir_synced = out.sync_parent().is_ok();
            Ok(report)
        }
        Err(e) => Err(e),
    }
}

/// Run the whole pipeline (validation, fold, `map`, verification) without
/// writing any output. Use it to see what a rewrite would do, and how many
/// records it would drop or replace, before running it for real.
pub fn dry_run<S, E, N>(
    src: &Path,
    old_reducer: impl Fn(&mut Tx, &E) -> Result<()>,
    mode: Mode,
    map: impl FnMut(Step<'_, E>) -> Result<Emit<N>>,
) -> Result<RewriteReport>
where
    S: Schema,
    E: DeserializeOwned,
    N: Serialize,
{
    let mut report = run::<S, E, N>(src, None, &old_reducer, mode, map)?;
    report.dry_run = true;
    Ok(report)
}

/// The output file under construction: a temp beside `dst`, published by
/// hard link. Dropping an unpublished output removes the temp.
struct Output {
    dst: PathBuf,
    tmp: PathBuf,
    file: Option<File>,
    published: bool,
}

impl Output {
    fn create(dst: &Path) -> Result<Self> {
        if fs::symlink_metadata(dst).is_ok() {
            return Err(Error::Log(format!(
                "rewrite refuses: destination {} already exists",
                dst.display()
            )));
        }
        let dir = parent_dir(dst);
        if !dir.is_dir() {
            return Err(Error::Log(format!(
                "rewrite destination directory {} does not exist",
                dir.display()
            )));
        }
        let name = dst
            .file_name()
            .ok_or_else(|| {
                Error::Log(format!(
                    "rewrite destination {} has no file name",
                    dst.display()
                ))
            })?
            .to_string_lossy()
            .into_owned();
        let tmp = dir.join(format!(".{name}.tmp-{}", crate::equiv::unique_suffix()));
        fault::hit("rewrite.create_temp")?;
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .map_err(|e| Error::Log(format!("create {}: {e}", tmp.display())))?;
        Ok(Self {
            dst: dst.to_path_buf(),
            tmp,
            file: Some(file),
            published: false,
        })
    }

    fn file_mut(&mut self) -> &mut File {
        self.file.as_mut().expect("output file open until publish")
    }

    /// Fsync the temp's content, then publish it at `dst` with a hard link,
    /// which fails if `dst` exists, then drop the temp name.
    fn publish(&mut self) -> Result<()> {
        if let Some(file) = self.file.take() {
            fault::hit("rewrite.publish.sync")?;
            file.sync_all()
                .map_err(|e| Error::Log(format!("fsync {}: {e}", self.tmp.display())))?;
        }
        fault::hit("rewrite.publish.link")?;
        fs::hard_link(&self.tmp, &self.dst).map_err(|e| {
            Error::Log(format!(
                "publish {} (refusing to overwrite): {e}",
                self.dst.display()
            ))
        })?;
        // Commit point passed: the destination now exists and is complete.
        self.published = true;
        let _ = fs::remove_file(&self.tmp);
        Ok(())
    }

    fn sync_parent(&self) -> std::io::Result<()> {
        File::open(parent_dir(&self.dst))?.sync_all()
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        if !self.published {
            self.file.take();
            let _ = fs::remove_file(&self.tmp);
        }
    }
}

fn parent_dir(path: &Path) -> PathBuf {
    match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

/// One record waiting for its batch to close.
struct Pending<N> {
    /// The source line, without its newline, exactly as read.
    line: Vec<u8>,
    env: SealedEnvelope,
    /// The raw event JSON, for re-sealing around unchanged event bytes.
    event: Vec<u8>,
    emit: Emit<N>,
}

fn run<S, E, N>(
    src: &Path,
    mut out: Option<&mut Output>,
    old_reducer: &impl Fn(&mut Tx, &E) -> Result<()>,
    mode: Mode,
    mut map: impl FnMut(Step<'_, E>) -> Result<Emit<N>>,
) -> Result<RewriteReport>
where
    S: Schema,
    E: DeserializeOwned,
    N: Serialize,
{
    let mut src_file = lock_tape_shared(src)?;
    let committed = scan_reader(BufReader::new(&src_file)).map_err(|c| {
        Error::Corruption(format!(
            "source tape corrupt at byte {}: {}",
            c.offset, c.reason
        ))
    })?;
    let file_len = src_file
        .metadata()
        .map_err(|e| Error::Log(e.to_string()))?
        .len();
    if file_len != committed.end as u64 {
        return Err(Error::Corruption(format!(
            "source tape has {} uncommitted tail bytes past seq {}: refusing to rewrite a tape \
             with a torn tail (opening it once with the writer stopped drops that residue)",
            file_len - committed.end as u64,
            committed.len
        )));
    }
    let mut report = RewriteReport {
        src_committed_bytes: committed.end as u64,
        ..Default::default()
    };
    src_file
        .seek(SeekFrom::Start(0))
        .map_err(|e| Error::Log(e.to_string()))?;

    let db = Db::open_memory()?;
    {
        let mut tx = db.write_with(Durability::DisableWal)?;
        write_stamp(&mut tx, ProjectionStamp::for_schema::<S>(0))?;
        tx.commit()?;
    }
    let mut reader = BufReader::new(&src_file);
    let mut batch: Vec<Pending<N>> = Vec::new();
    let mut out_seq = 0u64;
    let mut buf = Vec::new();
    let mut offset = 0usize;
    let mut line = Vec::new();
    while offset < committed.end {
        line.clear();
        let n = reader
            .read_until(b'\n', &mut line)
            .map_err(|e| Error::Log(e.to_string()))?;
        if n == 0 {
            break;
        }
        offset += n;
        let body = line.strip_suffix(b"\n").unwrap_or(&line);
        if body.iter().all(|b| b.is_ascii_whitespace()) {
            continue;
        }
        let (env, event_bytes) = parse_sealed(body)?;
        let event: E = serde_json::from_slice(&event_bytes).map_err(|e| {
            Error::Deserialize(format!(
                "seq {}: event does not decode as the source enum: {e}",
                env.seq
            ))
        })?;
        let rec = Record {
            seq: env.seq,
            ts_ms: env.ts_ms,
            from: env.from,
            to: env.to,
            event,
            crc: crc_of(body),
        };
        let emit = map(Step {
            rec: &rec,
            before: &db,
        })?;
        fold_one(&db, old_reducer, rec.meta(), &rec.event)?;
        report.read += 1;
        if matches!(emit, Emit::Drop) && mode == Mode::SameSeq {
            return Err(Error::Log(format!(
                "map returned Drop for seq {} in Mode::SameSeq (use Mode::Renumber)",
                env.seq
            )));
        }
        let seq = env.seq;
        let to = env.to;
        batch.push(Pending {
            line: body.to_vec(),
            env,
            event: event_bytes,
            emit,
        });
        if seq == to {
            buf.clear();
            flush_batch(&mut batch, mode, &mut out_seq, &mut buf, &mut report)?;
            if !buf.is_empty() {
                report.batches_written += 1;
                if let Some(o) = out.as_deref_mut() {
                    fault::hit("rewrite.write")?;
                    o.file_mut()
                        .write_all(&buf)
                        .map_err(|e| Error::Log(format!("write {}: {e}", o.tmp.display())))?;
                    report.dst_bytes += buf.len() as u64;
                }
            }
        }
    }
    if !batch.is_empty() {
        return Err(Error::Corruption(
            "committed prefix ends inside a batch (scan and reader disagree)".into(),
        ));
    }
    if let Some(o) = out.as_deref_mut() {
        o.file_mut()
            .flush()
            .map_err(|e| Error::Log(e.to_string()))?;
        verify_output(o, out_seq)?;
    }
    Ok(report)
}

/// Write one closed batch's output lines into `buf`, per `mode`.
fn flush_batch<N: Serialize>(
    batch: &mut Vec<Pending<N>>,
    mode: Mode,
    out_seq: &mut u64,
    buf: &mut Vec<u8>,
    report: &mut RewriteReport,
) -> Result<()> {
    let pending: Vec<Pending<N>> = std::mem::take(batch);
    match mode {
        Mode::SameSeq => {
            for p in pending {
                match p.emit {
                    Emit::Keep => {
                        buf.extend_from_slice(&p.line);
                    }
                    Emit::Replace(n) => {
                        buf.extend_from_slice(
                            render_sealed(p.env.seq, p.env.ts_ms, p.env.from, p.env.to, &n)?
                                .as_bytes(),
                        );
                        report.replaced += 1;
                    }
                    Emit::Drop => {
                        return Err(Error::Log(
                            "internal: Drop reached flush in Mode::SameSeq".into(),
                        ))
                    }
                }
                buf.push(b'\n');
                report.written += 1;
                *out_seq += 1;
            }
        }
        Mode::Renumber => {
            let mut kept = Vec::with_capacity(pending.len());
            for p in pending {
                if matches!(p.emit, Emit::Drop) {
                    report.dropped += 1;
                } else {
                    kept.push(p);
                }
            }
            if kept.is_empty() {
                return Ok(());
            }
            let from_out = *out_seq;
            let count = kept.len() as u64;
            let to_out = from_out + count - 1;
            for (i, p) in kept.into_iter().enumerate() {
                let seq = from_out + i as u64;
                match p.emit {
                    Emit::Keep => {
                        if seq == p.env.seq && from_out == p.env.from && to_out == p.env.to {
                            buf.extend_from_slice(&p.line);
                        } else {
                            buf.extend_from_slice(
                                render_sealed_raw(seq, p.env.ts_ms, from_out, to_out, &p.event)
                                    .as_bytes(),
                            );
                        }
                    }
                    Emit::Replace(n) => {
                        buf.extend_from_slice(
                            render_sealed(seq, p.env.ts_ms, from_out, to_out, &n)?.as_bytes(),
                        );
                        report.replaced += 1;
                    }
                    Emit::Drop => {
                        return Err(Error::Log("internal: Drop among kept records".into()))
                    }
                }
                buf.push(b'\n');
                report.written += 1;
            }
            *out_seq += count;
        }
    }
    Ok(())
}

/// Re-read the finished temp file and require a clean scan that ends exactly
/// at EOF with `expected` records. Only then may it be published.
fn verify_output(out: &mut Output, expected: u64) -> Result<()> {
    fault::hit("rewrite.verify")?;
    let file = File::open(&out.tmp).map_err(|e| Error::Log(e.to_string()))?;
    let size = file
        .metadata()
        .map_err(|e| Error::Log(e.to_string()))?
        .len() as usize;
    let found: Committed = scan_reader(BufReader::new(file)).map_err(|c| {
        Error::Corruption(format!(
            "rewritten tape fails its own scan at byte {}: {}",
            c.offset, c.reason
        ))
    })?;
    if found.end != size || found.len != expected {
        return Err(Error::Corruption(format!(
            "rewritten tape does not end at EOF cleanly (scan end {} of {size} bytes, \
             {} records of {expected} expected)",
            found.end, found.len
        )));
    }
    Ok(())
}

fn crc_of(body: &[u8]) -> String {
    String::from_utf8_lossy(&body[body.len() - CRC_SUFFIX_LEN + 8..body.len() - 2]).into_owned()
}

/// Stream every committed record of `path` (the committed prefix only),
/// decoding each event as `E`. Returns the scan result. Bounded memory: one
/// line at a time.
pub(crate) fn for_each_committed<E: DeserializeOwned>(
    path: &Path,
    mut f: impl FnMut(Record<E>) -> Result<()>,
) -> Result<Committed> {
    let mut file =
        File::open(path).map_err(|e| Error::Log(format!("open {}: {e}", path.display())))?;
    let committed = scan_reader(BufReader::new(&file)).map_err(|c| {
        Error::Corruption(format!("tape corrupt at byte {}: {}", c.offset, c.reason))
    })?;
    file.seek(SeekFrom::Start(0))
        .map_err(|e| Error::Log(e.to_string()))?;
    let mut reader = BufReader::new(&file);
    let mut offset = 0usize;
    let mut line = Vec::new();
    while offset < committed.end {
        line.clear();
        let n = reader
            .read_until(b'\n', &mut line)
            .map_err(|e| Error::Log(e.to_string()))?;
        if n == 0 {
            break;
        }
        offset += n;
        let body = line.strip_suffix(b"\n").unwrap_or(&line);
        if body.iter().all(|b| b.is_ascii_whitespace()) {
            continue;
        }
        let (env, event_bytes) = parse_sealed(body)?;
        let event: E = serde_json::from_slice(&event_bytes)
            .map_err(|e| Error::Deserialize(format!("seq {}: {e}", env.seq)))?;
        f(Record {
            seq: env.seq,
            ts_ms: env.ts_ms,
            from: env.from,
            to: env.to,
            event,
            crc: crc_of(body),
        })?;
    }
    Ok(committed)
}
