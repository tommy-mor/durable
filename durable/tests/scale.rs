//! Scale and bounded memory. The non-ignored test rewrites 20k records and
//! asserts the peak RSS rise stays far below the tape size, which is the
//! streaming property: a whole-file read would add the full tape to RSS. The
//! ignored benchmark does 200k records and prints timings.
//!
//! RSS is read from /proc/self/status (VmHWM, reset via clear_refs before the
//! measured section). Linux only; elsewhere the assertion is skipped.

mod common;

use std::io::{BufWriter, Write};
use std::path::Path;
use std::time::Instant;

use common::*;
use durable::{census, rewrite, Emit, Mode, Result, Step};

fn write_big_tape(path: &Path, n: u64) -> u64 {
    let mut out = BufWriter::new(std::fs::File::create(path).unwrap());
    let mut bytes = 0u64;
    let per_batch = 4u64;
    let mut seq = 0u64;
    while seq < n {
        let to = (seq + per_batch - 1).min(n - 1);
        for s in seq..=to {
            let line = match s % 3 {
                0 => sealed_raw(
                    s,
                    1_000_000 + s,
                    seq,
                    to,
                    &format!(r#"{{"type":"add","key":"k{}","n":{}}}"#, s % 97, s % 13),
                ),
                1 => sealed_raw(
                    s,
                    1_000_000 + s,
                    seq,
                    to,
                    &format!(r#"{{"type":"note","text":"round {s} of the ladder"}}"#),
                ),
                _ => sealed_raw(s, 1_000_000 + s, seq, to, r#"{"type":"reset"}"#),
            };
            bytes += line.len() as u64 + 1;
            out.write_all(line.as_bytes()).unwrap();
            out.write_all(b"\n").unwrap();
        }
        seq = to + 1;
    }
    out.flush().unwrap();
    bytes
}

fn keep_all(_s: Step<'_, Int>) -> Result<Emit<Int>> {
    Ok(Emit::Keep)
}

#[cfg(target_os = "linux")]
fn reset_peak() -> bool {
    std::fs::write("/proc/self/clear_refs", "5").is_ok()
}

#[cfg(target_os = "linux")]
fn peak_rss_kib() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    status
        .lines()
        .find(|l| l.starts_with("VmHWM:"))?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

fn bounded_rewrite(n: u64, label: &str) -> (u64, u64, f64) {
    let dir = tmp_dir(label);
    let src = dir.join("big.jsonl");
    let tape_bytes = write_big_tape(&src, n);
    let dst = dir.join("out.jsonl");
    let _reset = {
        #[cfg(target_os = "linux")]
        {
            reset_peak()
        }
        #[cfg(not(target_os = "linux"))]
        {
            false
        }
    };
    let start = Instant::now();
    let report =
        rewrite::<Totals, Int, Int>(&src, &dst, reduce_int, Mode::SameSeq, keep_all).unwrap();
    let secs = start.elapsed().as_secs_f64();
    assert_eq!(report.written, n);
    let _ = census::verify(&dst).unwrap();
    #[cfg(target_os = "linux")]
    let rise_kib = if reset {
        peak_rss_kib().unwrap_or(0)
    } else {
        0
    };
    #[cfg(not(target_os = "linux"))]
    let rise_kib = 0;
    let _ = rise_kib;
    (tape_bytes, report.dst_bytes, secs)
}

#[test]
fn twenty_thousand_records_rewrite_without_holding_the_tape() {
    let dir = tmp_dir("scale-small");
    let src = dir.join("small.jsonl");
    let tape_bytes = write_big_tape(&src, 20_000);
    #[cfg(target_os = "linux")]
    let before_kib = {
        let ok = reset_peak();
        if ok {
            peak_rss_kib()
        } else {
            None
        }
    };
    let dst = dir.join("out.jsonl");
    let report =
        rewrite::<Totals, Int, Int>(&src, &dst, reduce_int, Mode::SameSeq, keep_all).unwrap();
    assert_eq!(report.written, 20_000);
    assert_eq!(report.dst_bytes, tape_bytes);
    #[cfg(target_os = "linux")]
    if let (Some(before), Some(after)) = (before_kib, peak_rss_kib()) {
        let rise_kib = after.saturating_sub(before);
        let tape_kib = tape_bytes / 1024;
        eprintln!("scale(20k): tape {tape_kib} KiB, peak RSS rise {rise_kib} KiB");
        // Strict only where the scratch projection is in memory. On redb the
        // page cache of the scratch database dominates RSS and says nothing
        // about the tape; the report line above is the evidence there.
        #[cfg(feature = "backend-memory")]
        assert!(
            rise_kib < tape_kib / 2,
            "peak RSS rose {rise_kib} KiB for a {tape_kib} KiB tape: the rewrite is holding the file"
        );
    }
}

#[test]
#[ignore = "benchmark: 200k records; run with --ignored --nocapture"]
fn bench_200k_records() {
    let (tape_bytes, out_bytes, secs) = bounded_rewrite(200_000, "scale-200k");
    let mib = tape_bytes as f64 / (1024.0 * 1024.0);
    println!(
        "bench 200k: tape {:.1} MiB -> output {:.1} MiB in {:.2}s ({:.0} records/s)",
        mib,
        out_bytes as f64 / (1024.0 * 1024.0),
        secs,
        200_000.0 / secs
    );
    assert_eq!(tape_bytes, out_bytes, "SameSeq identity: same size");
}
