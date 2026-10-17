//! Code-review repros for tape slices 1–3a. Each test asserts the
//! behaviour the spec requires; a failure is a confirmed bug.

use durable::{Durable, Leaf, Map, Runtime};
use serde::{Deserialize, Serialize};
use std::path::Path;
use tempfile::TempDir;

#[derive(Durable)]
#[allow(dead_code)]
struct Store {
    seen: Map<String, Leaf<String>>,
}
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
struct Ev {
    note: String,
}
fn reduce(tx: &mut durable::Tx, e: &Ev) -> durable::Result<()> {
    tx.write(Store::root().seen().key(&e.note).set(&e.note));
    Ok(())
}
fn open(dir: &Path) -> durable::Result<Runtime<Ev>> {
    Runtime::open_described::<Store>(dir.join("proj"), dir.join("log.jsonl"), None, reduce)
}
fn ev(s: &str) -> Ev {
    Ev { note: s.into() }
}

/// No legacy path, ever, by Master's law: unsealed lines are not
/// candidates. A pre-seal tape opens to the sealed prefix (here zero),
/// its residue drops at seal, and the runtime rises empty ready to take
/// sealed seed. Old tapes migrate through the offline migration script.
#[test]
fn a_unsealed_tape_is_residue_not_history() {
    let dir = TempDir::new().unwrap();
    let legacy = "{\"seq\":0,\"ts_ms\":1,\"event\":{\"note\":\"a\"}}\n\
                  {\"seq\":1,\"ts_ms\":2,\"event\":{\"note\":\"b\"}}\n";
    std::fs::write(dir.path().join("log.jsonl"), legacy).unwrap();
    let rt = open(dir.path()).expect("unsealed residue must open as empty");
    assert_eq!(rt.applied().unwrap(), 0);
    assert_eq!(std::fs::read_to_string(dir.path().join("log.jsonl")).unwrap(), "");
    rt.append(ev("fresh")).unwrap();
    assert_eq!(rt.applied().unwrap(), 1);
    rt.verify().unwrap();
}

/// Spec, Opening step 4: cross-check BEFORE truncating. Damage the final
/// acknowledged batch while the projection has applied it: open must
/// refuse with disagree and leave the file byte-identical.
#[test]
fn c_disagree_leaves_file_untouched() {
    let dir = TempDir::new().unwrap();
    {
        let rt = open(dir.path()).unwrap();
        for n in ["a", "b", "c"] {
            rt.append(ev(n)).unwrap();
        }
    }
    let log = dir.path().join("log.jsonl");
    let mut bytes = std::fs::read(&log).unwrap();
    let n = bytes.len();
    bytes[n - 4] = if bytes[n - 4] == b'0' { b'1' } else { b'0' }; // flip last crc hex
    std::fs::write(&log, &bytes).unwrap();
    let err = open(dir.path()).err().expect("should refuse").to_string();
    assert!(err.contains("disagree"), "{err}");
    assert_eq!(std::fs::read(&log).unwrap(), bytes, "file was truncated before the cross-check");
}

/// A failed/partial write leaves bytes behind and `len` unchanged (no
/// rollback, no poison). The next append is acknowledged, then lost on
/// reopen because its `from` equals the committed len and it reads as
/// residue. Simulated by dropping a partial line onto the live tape.
#[test]
fn d_ack_after_partial_write_survives_reopen() {
    let dir = TempDir::new().unwrap();
    let log = dir.path().join("log.jsonl");
    {
        let rt = open(dir.path()).unwrap();
        rt.append(ev("a")).unwrap();
        // what write_all leaves when ENOSPC hits mid-line:
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(&log).unwrap();
        f.write_all(b"{\"seq\":1,\"ts_ms\":17").unwrap();
        let rec = rt.append(ev("b")).unwrap(); // acknowledged
        assert_eq!(rec.seq, 1);
    }
    let rt = open(dir.path()).expect("reopen after an acknowledged append");
    assert_eq!(rt.applied().unwrap(), 2, "acknowledged event b lost");
}

/// append_batch accepts batches larger than MAX_SEALED_BATCH, but the
/// scan rejects every line of them, so an acknowledged batch is erased.
#[test]
fn e_oversize_batch_survives_reopen() {
    let dir = TempDir::new().unwrap();
    {
        let rt = open(dir.path()).unwrap();
        let evs: Vec<Ev> = (0..1100).map(|i| ev(&i.to_string())).collect();
        if rt.append_batch(evs).is_err() {
            return; // rejecting up front is also correct
        }
    }
    let len_before = std::fs::metadata(dir.path().join("log.jsonl")).unwrap().len();
    let res = open(dir.path());
    let len_after = std::fs::metadata(dir.path().join("log.jsonl")).unwrap().len();
    assert_eq!(len_after, len_before, "acknowledged batch truncated (open: {:?})", res.err().map(|e| e.to_string()));
}

// ---- round 2 -------------------------------------------------------------

/// A sealed tape whose very first append was torn (fresh deploy, disk
/// full on event 0) has committed len 0 and non-empty bytes. That is
/// in-flight residue, not a legacy tape: open must drop it and rise empty.
#[test]
fn f_torn_first_sealed_batch_is_residue_not_legacy() {
    let dir = TempDir::new().unwrap();
    {
        let rt = open(dir.path()).unwrap();
        rt.append(ev("a")).unwrap();
    }
    let log = dir.path().join("log.jsonl");
    let bytes = std::fs::read(&log).unwrap();
    std::fs::write(&log, &bytes[..bytes.len() / 2]).unwrap(); // torn, no newline
    std::fs::remove_dir_all(dir.path().join("proj")).unwrap(); // projection never applied it
    let rt = open(dir.path()).expect("torn first sealed batch must open as empty");
    assert_eq!(rt.applied().unwrap(), 0);
}

/// A mixed tape (sealed prefix, unsealed tail) keeps its sealed history:
/// the scan commits the sealed batches and drops the unsealed residue.
#[test]
fn g_sealed_prefix_survives_unsealed_tail() {
    let dir = TempDir::new().unwrap();
    {
        let rt = open(dir.path()).unwrap();
        rt.append(ev("a")).unwrap();
        rt.append(ev("b")).unwrap();
    }
    let log = dir.path().join("log.jsonl");
    let mut bytes = std::fs::read(&log).unwrap();
    bytes.extend_from_slice(b"{\"seq\":99,\"ts_ms\":1,\"event\":{\"note\":\"ghost\"}}\n");
    std::fs::write(&log, &bytes).unwrap();
    let rt = open(dir.path()).expect("sealed prefix must survive");
    assert_eq!(rt.applied().unwrap(), 2);
    let recs = rt.records_from(0).unwrap();
    assert_eq!(recs.len(), 2);
    rt.verify().unwrap();
}

/// Every append re-reads and re-scans the whole tape (scan_file inside
/// append_all), so a single append's cost grows with tape size.
#[test]
fn h_append_cost_is_independent_of_tape_size() {
    let dir = TempDir::new().unwrap();
    let rt = open(dir.path()).unwrap();
    let pad = "x".repeat(2000);
    let time = |rt: &Runtime<Ev>, n: usize, tag: &str| {
        let t = std::time::Instant::now();
        for i in 0..n {
            rt.append(ev(&format!("{tag}{i}{pad}"))).unwrap();
        }
        t.elapsed() / n as u32
    };
    let early = time(&rt, 200, "e");
    for chunk in 0..20 {
        let evs = (0..1000).map(|i| ev(&format!("m{chunk}-{i}{pad}"))).collect();
        rt.append_batch(evs).unwrap();
    }
    let late = time(&rt, 200, "l");
    eprintln!("per-append early={early:?} late={late:?} (tape ~{} MB)", 20_400 * 2 / 1000);
    assert!(late < early * 4, "append slowed from {early:?} to {late:?} as the tape grew");
}
