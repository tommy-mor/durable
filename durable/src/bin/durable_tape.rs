//! `durable-tape`: read-only operator commands for a sealed tape.
//!
//! ```text
//! durable-tape status <tape>
//! durable-tape verify <tape>
//! durable-tape cat <tape> [--from SEQ]
//! durable-tape census <tape> [--tag KEY] [--json]
//! ```
//!
//! Exit codes: 0 success, 1 the tape is corrupt or unreadable, 2 usage,
//! 4 `verify` found a committed prefix plus a torn tail (not corrupt, not clean).
//! Nothing here writes, truncates or locks the tape; a torn tail is reported.

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use durable::census::{self, status, verify};

const USAGE: &str = "usage:
  durable-tape status <tape>
  durable-tape verify <tape>
  durable-tape cat <tape> [--from SEQ]
  durable-tape census <tape> [--tag KEY] [--json]";

fn usage(msg: &str) -> ExitCode {
    eprintln!("durable-tape: {msg}\n{USAGE}");
    ExitCode::from(2)
}

fn fail(e: durable::Error) -> ExitCode {
    eprintln!("durable-tape: {e}");
    ExitCode::from(1)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some((cmd, rest)) = args.split_first() else {
        return usage("missing command");
    };
    match cmd.as_str() {
        "status" => {
            let Some(tape) = one_path(rest) else {
                return usage("status takes exactly one tape");
            };
            match status(&tape) {
                Ok(s) => {
                    let c = s.committed;
                    println!("tape: {}", tape.display());
                    println!("sealed: yes");
                    println!("committed_end: {}", c.end);
                    println!("records: {}", c.len);
                    if c.len > 0 {
                        println!("last_seq: {}", c.len - 1);
                    }
                    println!("last_ts_ms: {}", c.last_ts);
                    println!("tail_bytes: {}", s.tape_bytes - c.end as u64);
                    ExitCode::SUCCESS
                }
                Err(e) => fail(e),
            }
        }
        "verify" => {
            let Some(tape) = one_path(rest) else {
                return usage("verify takes exactly one tape");
            };
            match verify(&tape) {
                Ok(v) if v.tail_bytes == 0 => {
                    println!(
                        "ok: {} records, committed_end {}",
                        v.records, v.committed_end
                    );
                    ExitCode::SUCCESS
                }
                Ok(v) => {
                    // Committed prefix is sound; the bytes after it are a torn
                    // or in-flight write. Opening the tape with the writer
                    // stopped seals (drops) them; say so, don't pretend clean.
                    println!(
                        "torn tail: {} records committed (end {}), {} uncommitted bytes past it",
                        v.records, v.committed_end, v.tail_bytes
                    );
                    ExitCode::from(4)
                }
                Err(e) => fail(e),
            }
        }
        "cat" => {
            let mut from = 0u64;
            let mut paths = Vec::new();
            let mut it = rest.iter();
            while let Some(a) = it.next() {
                if a == "--from" {
                    match it.next().and_then(|v| v.parse().ok()) {
                        Some(n) => from = n,
                        None => return usage("--from needs a sequence number"),
                    }
                } else {
                    paths.push(PathBuf::from(a));
                }
            }
            let [tape] = paths.as_slice() else {
                return usage("cat takes exactly one tape");
            };
            match census_cat(tape, from) {
                Ok(()) => ExitCode::SUCCESS,
                Err(code) => code,
            }
        }
        "census" => {
            let mut tag = "type".to_string();
            let mut json = false;
            let mut paths = Vec::new();
            let mut it = rest.iter();
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--json" => json = true,
                    "--tag" => match it.next() {
                        Some(t) => tag = t.clone(),
                        None => return usage("--tag needs a key"),
                    },
                    _ => paths.push(PathBuf::from(a)),
                }
            }
            let [tape] = paths.as_slice() else {
                return usage("census takes exactly one tape");
            };
            match census::census(tape, &tag) {
                Ok(c) => {
                    if json {
                        println!("{}", c.to_json());
                    } else {
                        print!("{}", c.to_table());
                    }
                    ExitCode::SUCCESS
                }
                Err(e) => fail(e),
            }
        }
        "-h" | "--help" | "help" => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        other => usage(&format!("unknown command {other:?}")),
    }
}

fn one_path(rest: &[String]) -> Option<PathBuf> {
    match rest {
        [p] => Some(PathBuf::from(p)),
        _ => None,
    }
}

fn census_cat(tape: &std::path::Path, from: u64) -> Result<(), ExitCode> {
    let lines = census::cat(tape, from).map_err(fail)?;
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for line in lines {
        if out
            .write_all(&line)
            .and_then(|_| out.write_all(b"\n"))
            .is_err()
        {
            return Err(ExitCode::from(1));
        }
    }
    Ok(())
}
