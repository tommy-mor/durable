//! durable-tape-migrate — offline migration of a pre-seal JSONL tape.
//!
//! Usage: durable-tape-migrate [--drop-torn-tail] <tape.jsonl>
//!
//! Reads old `{seq, ts_ms, event}` lines and writes sealed
//! `{seq, ts_ms, from=seq, to=seq, event, crc}` lines to `<tape>.migrated`
//! (fsynced; the operator renames it over the tape while no writer runs).
//! Master's law: there is no legacy path in the writer — old tapes cross
//! over through this blade, never through open.
//!
//! Rules, hard as stone:
//! - every line must parse with u64 seq/ts_ms and a present event, seqs
//!   contiguous from 0; ANY damaged line refuses with a non-zero exit and
//!   migrates NOTHING — the operator decides, the blade never judges;
//! - a torn tail (final bytes with no newline) also refuses, unless the
//!   operator passes `--drop-torn-tail`. The old writer fsynced only after
//!   whole lines, so an unterminated final fragment was never acknowledged;
//!   the flag drops exactly that fragment and prints it. A damaged line
//!   that IS terminated is acknowledged history and is never excused;
//! - each event's bytes travel VERBATIM (RawValue carry): re-serializing
//!   through a JSON value would sort keys and turn integers past 64 bits
//!   into lossy floats;
//! - blank lines are skipped, matching the scan's tolerance.

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use durable::runtime::render_sealed;

fn fail(msg: &str) -> ExitCode {
    eprintln!("durable-tape-migrate: {msg}");
    ExitCode::from(2)
}

fn migrate(path: &Path, drop_torn_tail: bool) -> Result<PathBuf, String> {
    let bytes =
        std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let mut out = String::new();
    let mut expected_seq = 0u64;
    let mut lineno = 0u64;
    // Split keeping terminators: a final chunk without \n is a torn tail.
    let mut start = 0usize;
    while start < bytes.len() {
        match bytes[start..].iter().position(|b| *b == b'\n') {
            Some(i) => {
                lineno += 1;
                let chunk = &bytes[start..start + i];
                start += i + 1;
                if chunk.is_empty() || chunk.iter().all(|b| b.is_ascii_whitespace()) {
                    continue;
                }
                let (seq, ts, event) = parse_old_line(chunk)
                    .map_err(|e| format!("line {lineno}: {e}"))?;
                if seq != expected_seq {
                    return Err(format!(
                        "line {lineno}: non-contiguous seq (expected {expected_seq}, got {seq})"
                    ));
                }
                out.push_str(
                    &render_sealed(seq, ts, seq, seq, &event)
                        .map_err(|e| format!("line {lineno}: seal failed: {e}"))?,
                );
                out.push('\n');
                expected_seq += 1;
            }
            None => {
                let tail = &bytes[start..];
                if !tail.is_empty() && !tail.iter().all(|b| b.is_ascii_whitespace()) {
                    if !drop_torn_tail {
                        return Err(format!(
                            "torn tail past line {lineno}: {} bytes with no newline — migrate nothing \
                             (rerun with --drop-torn-tail to drop exactly this fragment)",
                            tail.len()
                        ));
                    }
                    eprintln!(
                        "durable-tape-migrate: dropping torn tail past line {lineno} ({} bytes, never acknowledged): {}",
                        tail.len(),
                        String::from_utf8_lossy(tail).escape_debug()
                    );
                }
                break;
            }
        }
    }
    if expected_seq == 0 {
        return Err("empty tape: nothing to migrate".into());
    }
    let dest = path.with_extension("migrated");
    std::fs::write(&dest, out.as_bytes())
        .map_err(|e| format!("write {}: {e}", dest.display()))?;
    let f = OpenOptions::new()
        .read(true)
        .open(&dest)
        .map_err(|e| format!("open {}: {e}", dest.display()))?;
    f.sync_data()
        .map_err(|e| format!("fsync {}: {e}", dest.display()))?;
    Ok(dest)
}

/// An old line: `{seq, ts_ms, event}` with NO sealed envelope. The event
/// rides out as a verbatim RawValue — never re-serialized.
fn parse_old_line(
    line: &[u8],
) -> Result<(u64, u64, Box<serde_json::value::RawValue>), String> {
    #[derive(serde::Deserialize)]
    struct Old<'a> {
        seq: u64,
        ts_ms: u64,
        #[serde(borrow)]
        event: &'a serde_json::value::RawValue,
        // Envelope fields only a sealed line has. Checked at the top level,
        // so an event that carries its own "crc" field is still old.
        crc: Option<serde::de::IgnoredAny>,
        from: Option<serde::de::IgnoredAny>,
        to: Option<serde::de::IgnoredAny>,
    }
    let text =
        std::str::from_utf8(line).map_err(|e| format!("not utf-8: {e}"))?;
    let old: Old =
        serde_json::from_str(text).map_err(|e| format!("not an old record: {e}"))?;
    if old.crc.is_some() || old.from.is_some() || old.to.is_some() {
        return Err("already sealed (envelope has crc/from/to) — refusing to re-migrate".into());
    }
    // Re-own the verbatim bytes: RawValue borrows the input line, which
    // the caller drops. from_string validates without re-rendering.
    let owned = serde_json::value::RawValue::from_string(old.event.get().to_string())
        .map_err(|e| format!("event not verbatim-carriable: {e}"))?;
    Ok((old.seq, old.ts_ms, owned))
}

fn main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let drop_torn_tail = if let Some(i) = args.iter().position(|a| a == "--drop-torn-tail") {
        args.remove(i);
        true
    } else {
        false
    };
    if args.len() != 1 || args[0] == "-h" || args[0] == "--help" {
        eprintln!("usage: durable-tape-migrate [--drop-torn-tail] <tape.jsonl>");
        eprintln!("writes <tape>.migrated (fsynced); with no writer running, rename it over the tape");
        eprintln!("and fsync the directory (e.g. `mv` then `sync`). The existing projection stays valid.");
        return ExitCode::from(if args.len() != 1 { 2 } else { 0 });
    }
    match migrate(Path::new(&args[0]), drop_torn_tail) {
        Ok(dest) => {
            println!("migrated -> {}", dest.display());
            ExitCode::SUCCESS
        }
        Err(e) => fail(&e),
    }
}
