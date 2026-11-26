//! Read-only inspection of a sealed tape: `status`, `verify`, `cat` and
//! `census`, as the library functions behind the `durable-tape` binary.
//!
//! Nothing here writes, truncates or locks the tape. A torn tail is reported,
//! never removed. Event kinds are detected from the JSON, not from a Rust type,
//! so census works on any tape without the application's enum:
//!
//! * internally tagged (`#[serde(tag = "type")]`): the value of the tag key
//!   (`--tag`, default `type`) is the kind; the other top-level keys are fields;
//! * externally tagged (serde's default): an event that is a bare string is a
//!   unit variant whose name is the kind; an object with exactly one key is a
//!   variant whose name is that key; its fields are the keys of its object
//!   payload, if the payload is an object;
//! * adjacently tagged (`{"t": "...", "c": {...}}`): pass the tag key as
//!   `--tag t`; the content key then shows up as a field;
//! * untagged, or anything else: counted under `untagged`, with its top-level
//!   keys as fields when it is an object.
//!
//! Detection is a heuristic over JSON shape. An externally tagged enum with a
//! unit variant literally named like the tag key (`type`) is misread as a
//! tagged event when the tag is `type`. Use `--tag` with a key your events
//! never use to avoid that.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde::de::IgnoredAny;

use crate::runtime::{classify, scan_reader, Classified, Committed};
use crate::{Error, Result};

/// Machine-readable schema identifier for `census --json`.
pub const CENSUS_SCHEMA: &str = "durable-census/1";

/// One event kind's totals.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KindCensus {
    pub count: u64,
    /// Sum of the raw event JSON bytes for this kind.
    pub event_bytes: u64,
    /// Records of this kind in which each top-level field was present.
    pub fields: BTreeMap<String, u64>,
}

/// Everything `census` reports about a tape's committed prefix.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Census {
    pub tape: PathBuf,
    pub tag: String,
    pub tape_bytes: u64,
    pub committed_bytes: u64,
    /// Bytes past the committed end (an in-flight torn tail).
    pub tail_bytes: u64,
    pub records: u64,
    pub batches: u64,
    pub first_seq: Option<u64>,
    pub last_seq: Option<u64>,
    pub ts_min: Option<u64>,
    pub ts_max: Option<u64>,
    /// Batch size (`to - from + 1`) to number of batches of that size.
    pub batch_sizes: BTreeMap<u64, u64>,
    /// Sum of the raw event JSON bytes over every record.
    pub event_bytes: u64,
    /// Records whose event carried no kind (see the module docs).
    pub untagged: u64,
    pub kinds: BTreeMap<String, KindCensus>,
}

/// Detect `(kind, fields)` for one event. `kind` is `None` when the event has
/// no detectable kind.
pub fn detect_kind(event: &serde_json::Value, tag: &str) -> (Option<String>, Vec<String>) {
    use serde_json::Value;
    match event {
        Value::String(s) => (Some(s.clone()), Vec::new()),
        Value::Object(map) => {
            if let Some(Value::String(kind)) = map.get(tag) {
                let fields = map.keys().filter(|k| k.as_str() != tag).cloned().collect();
                return (Some(kind.clone()), fields);
            }
            if map.len() == 1 {
                let (key, payload) = map.iter().next().expect("one entry");
                let fields = match payload {
                    Value::Object(inner) => inner.keys().cloned().collect(),
                    _ => Vec::new(),
                };
                return (Some(key.clone()), fields);
            }
            (None, map.keys().cloned().collect())
        }
        _ => (None, Vec::new()),
    }
}

/// Read-only census of the committed prefix of `path`, tagging events by `tag`.
///
/// Errors with [`Error::Corruption`] when the tape does not scan clean (the
/// census never guesses past damage), and [`Error::Log`] on I/O failure.
pub fn census(path: &Path, tag: &str) -> Result<Census> {
    let mut file = open_read(path)?;
    let tape_bytes = file.metadata().map_err(io)?.len();
    let committed = scan(&file)?;
    let mut out = Census {
        tape: path.to_path_buf(),
        tag: tag.to_string(),
        tape_bytes,
        committed_bytes: committed.end as u64,
        tail_bytes: tape_bytes - committed.end as u64,
        ..Default::default()
    };
    for_each_line(&mut file, committed, |body| {
        let (env, event_bytes) = match classify(body) {
            Classified::Candidate(env, event) => (env, event),
            _ => {
                return Err(Error::Corruption(
                    "committed line failed classification (scan and census disagree)".into(),
                ))
            }
        };
        out.records += 1;
        out.first_seq.get_or_insert(env.seq);
        out.last_seq = Some(env.seq);
        out.ts_min = Some(out.ts_min.map_or(env.ts_ms, |m| m.min(env.ts_ms)));
        out.ts_max = Some(out.ts_max.map_or(env.ts_ms, |m| m.max(env.ts_ms)));
        out.event_bytes += event_bytes.len() as u64;
        if env.seq == env.to {
            out.batches += 1;
            *out.batch_sizes.entry(env.to - env.from + 1).or_insert(0) += 1;
        }
        let value: serde_json::Value = serde_json::from_slice(event_bytes)
            .map_err(|e| Error::Deserialize(format!("seq {}: event is not JSON: {e}", env.seq)))?;
        match detect_kind(&value, tag) {
            (Some(kind), fields) => {
                let entry = out.kinds.entry(kind).or_default();
                entry.count += 1;
                entry.event_bytes += event_bytes.len() as u64;
                for field in fields {
                    *entry.fields.entry(field).or_insert(0) += 1;
                }
            }
            (None, _) => out.untagged += 1,
        }
        Ok(())
    })?;
    Ok(out)
}

impl Census {
    /// The `--json` form. Keys are sorted (BTreeMap), so output is stable.
    pub fn to_json(&self) -> serde_json::Value {
        use serde_json::{json, Map, Value};
        let kinds: Map<String, Value> = self
            .kinds
            .iter()
            .map(|(k, v)| {
                let fields: Map<String, Value> = v
                    .fields
                    .iter()
                    .map(|(f, n)| (f.clone(), json!(n)))
                    .collect();
                (
                    k.clone(),
                    json!({"count": v.count, "event_bytes": v.event_bytes, "fields": fields}),
                )
            })
            .collect();
        let sizes: Map<String, Value> = self
            .batch_sizes
            .iter()
            .map(|(s, n)| (s.to_string(), json!(n)))
            .collect();
        json!({
            "schema": CENSUS_SCHEMA,
            "tape": self.tape.display().to_string(),
            "tag": self.tag,
            "bytes": {
                "tape": self.tape_bytes,
                "committed": self.committed_bytes,
                "tail": self.tail_bytes,
                "event_json": self.event_bytes,
            },
            "records": self.records,
            "batches": self.batches,
            "seq": self.first_seq.map(|f| json!({"first": f, "last": self.last_seq})),
            "ts_ms": self.ts_min.map(|m| json!({"min": m, "max": self.ts_max})),
            "batch_sizes": sizes,
            "untagged": self.untagged,
            "kinds": kinds,
        })
    }

    /// The human table.
    pub fn to_table(&self) -> String {
        let mut s = String::new();
        s.push_str(&format!("tape      {}\n", self.tape.display()));
        s.push_str(&format!(
            "bytes     tape {}  committed {}  tail {}  event-json {}\n",
            self.tape_bytes, self.committed_bytes, self.tail_bytes, self.event_bytes
        ));
        s.push_str(&format!(
            "records   {}   batches {}\n",
            self.records, self.batches
        ));
        match (self.first_seq, self.last_seq) {
            (Some(f), Some(l)) => s.push_str(&format!("seq       {f}..{l}\n")),
            _ => s.push_str("seq       (empty)\n"),
        }
        if let (Some(a), Some(b)) = (self.ts_min, self.ts_max) {
            s.push_str(&format!("ts_ms     {a}..{b}\n"));
        }
        let sizes: Vec<String> = self
            .batch_sizes
            .iter()
            .map(|(s, n)| format!("{s}x{n}"))
            .collect();
        s.push_str(&format!(
            "batch     sizes {}\n",
            if sizes.is_empty() {
                "-".into()
            } else {
                sizes.join(" ")
            }
        ));
        s.push_str(&format!("tag       {}\n", self.tag));
        s.push_str(&format!("untagged  {}\n\n", self.untagged));
        let width = self.kinds.keys().map(|k| k.len()).max().unwrap_or(4).max(4);
        s.push_str(&format!(
            "{:<width$}  {:>7}  {:>12}  fields\n",
            "kind", "count", "event-bytes"
        ));
        for (kind, k) in &self.kinds {
            let fields: Vec<String> = k.fields.iter().map(|(f, n)| format!("{f}={n}")).collect();
            s.push_str(&format!(
                "{:<width$}  {:>7}  {:>12}  {}\n",
                kind,
                k.count,
                k.event_bytes,
                if fields.is_empty() {
                    "-".into()
                } else {
                    fields.join(" ")
                }
            ));
        }
        s
    }
}

/// `status`: where the committed prefix ends and what sits past it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Status {
    pub tape_bytes: u64,
    pub committed: Committed,
}

pub fn status(path: &Path) -> Result<Status> {
    let file = open_read(path)?;
    let tape_bytes = file.metadata().map_err(io)?.len();
    let committed = scan(&file)?;
    Ok(Status {
        tape_bytes,
        committed,
    })
}

/// `verify`: a full structural pass. The scan checks every committed envelope
/// and CRC; this additionally requires every committed event to be JSON.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Verified {
    pub records: u64,
    pub committed_end: u64,
    pub tail_bytes: u64,
}

pub fn verify(path: &Path) -> Result<Verified> {
    let mut file = open_read(path)?;
    let tape_bytes = file.metadata().map_err(io)?.len();
    let committed = scan(&file)?;
    for_each_line(&mut file, committed, |body| match classify(body) {
        Classified::Candidate(_, event) => {
            serde_json::from_slice::<IgnoredAny>(event)
                .map_err(|e| Error::Deserialize(format!("event is not JSON: {e}")))?;
            Ok(())
        }
        _ => Err(Error::Corruption(
            "committed line failed classification".into(),
        )),
    })?;
    Ok(Verified {
        records: committed.len,
        committed_end: committed.end as u64,
        tail_bytes: tape_bytes - committed.end as u64,
    })
}

/// `cat`: the committed lines with `seq >= from`, exactly as they sit on the
/// tape (newline excluded), in order.
pub fn cat(path: &Path, from: u64) -> Result<Vec<Vec<u8>>> {
    let mut file = open_read(path)?;
    let committed = scan(&file)?;
    let mut lines = Vec::new();
    for_each_line(&mut file, committed, |body| {
        if let Classified::Candidate(env, _) = classify(body) {
            if env.seq >= from {
                lines.push(body.to_vec());
            }
        }
        Ok(())
    })?;
    Ok(lines)
}

fn open_read(path: &Path) -> Result<File> {
    File::open(path).map_err(|e| Error::Log(format!("open {}: {e}", path.display())))
}

fn io(e: std::io::Error) -> Error {
    Error::Log(e.to_string())
}

fn scan(file: &File) -> Result<Committed> {
    scan_reader(BufReader::new(file))
        .map_err(|c| Error::Corruption(format!("tape corrupt at byte {}: {}", c.offset, c.reason)))
}

/// Visit each committed, non-blank line body (newline stripped), in order.
fn for_each_line(
    file: &mut File,
    committed: Committed,
    mut f: impl FnMut(&[u8]) -> Result<()>,
) -> Result<()> {
    file.seek(SeekFrom::Start(0)).map_err(io)?;
    let mut reader = BufReader::new(&*file);
    let mut offset = 0usize;
    let mut line = Vec::new();
    while offset < committed.end {
        line.clear();
        let n = reader.read_until(b'\n', &mut line).map_err(io)?;
        if n == 0 {
            break;
        }
        offset += n;
        let body = line.strip_suffix(b"\n").unwrap_or(&line);
        if body.iter().all(|b| b.is_ascii_whitespace()) {
            continue;
        }
        f(body)?;
    }
    Ok(())
}
