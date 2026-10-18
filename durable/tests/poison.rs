//! Outcome table: every poison and failure branch of the sealed writer,
//! driven through JsonlLog with a failing double over an in-memory disk.
//! Each trial scans the disk the writer left behind, so a rollback is
//! proven by bytes, not by an error string.

use durable::runtime::{render_sealed, scan};
use durable::{FailMode, FailingFile, FakeDisk, JsonlLog, Record};
use tempfile::TempDir;

type Log = JsonlLog<Record<serde_json::Value>>;

fn rec(seq: u64, note: &str) -> Record<serde_json::Value> {
    Record {
        seq,
        ts_ms: 1000 + seq,
        from: seq,
        to: seq,
        event: serde_json::json!({"note": note}),
        crc: String::new(),
    }
}

/// A live log with one committed batch, its handle swapped for a double
/// whose disk starts as the real tape's bytes (plus `extra`). Returns the
/// log, the double's disk, and the committed bytes.
fn rigged(mode: Option<FailMode>, extra: &[u8]) -> (TempDir, Log, FakeDisk, Vec<u8>) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("log.jsonl");
    let line = render_sealed(0, 1000, 0, 0, &serde_json::json!({"note": "a"})).unwrap();
    let committed = format!("{line}\n").into_bytes();
    std::fs::write(&path, &committed).unwrap();
    let (mut log, found) = Log::open_sealed(&path).unwrap();
    assert_eq!(found.len, 1);
    let start = [committed.clone(), extra.to_vec()].concat();
    let (fail, disk) = FailingFile::over(start, mode);
    log.inject_failing_file(fail);
    (dir, log, disk, committed)
}

fn disk_bytes(disk: &FakeDisk) -> Vec<u8> {
    disk.lock().unwrap().clone()
}

#[test]
fn n1_write_enospc_landing_nothing_is_not_committed_and_healthy() {
    let (_d, mut log, disk, committed) = rigged(Some(FailMode::WriteEnospc), b"");
    let err = log.append_all(&[rec(1, "b")]).unwrap_err().to_string();
    assert!(err.contains("not committed"), "{err}");
    assert!(!log.is_poisoned());
    assert_eq!(disk_bytes(&disk), committed);
}

#[test]
fn n1b_partial_write_enospc_rolls_back_and_stays_healthy() {
    // The production failure: a full disk lands part of the batch.
    let (_d, mut log, disk, committed) = rigged(Some(FailMode::WritePartialEnospc(17)), b"");
    let err = log.append_all(&[rec(1, "b"), rec(2, "c")]).unwrap_err().to_string();
    assert!(err.contains("not committed"), "{err}");
    assert!(!log.is_poisoned(), "a confirmed rollback must not poison");
    assert_eq!(disk_bytes(&disk), committed, "partial bytes were not rolled back");
}

#[test]
fn n2_partial_write_with_failed_rollback_poisons() {
    let (_d, mut log, disk, committed) =
        rigged(Some(FailMode::WritePartialEnospcAndTruncateEperm(17)), b"");
    let err = log.append_all(&[rec(1, "b")]).unwrap_err().to_string();
    assert!(err.contains("poisoned"), "{err}");
    assert!(log.is_poisoned());
    // The partial bytes are stuck on disk, but they are unclosed residue:
    // the scan still reads exactly the committed prefix.
    let bytes = disk_bytes(&disk);
    assert!(bytes.len() > committed.len());
    assert_eq!(scan(&bytes).unwrap().len, 1);
}

#[test]
fn n2b_residue_with_failed_truncate_poisons_before_writing() {
    let (_d, mut log, disk, _) = rigged(Some(FailMode::TruncateEperm), b"PARTIAL-NO-NEWLINE");
    let before = disk_bytes(&disk);
    let err = log.append_all(&[rec(1, "b")]).unwrap_err().to_string();
    assert!(err.contains("poisoned"), "{err}");
    assert!(log.is_poisoned());
    assert_eq!(disk_bytes(&disk), before, "must not write behind residue it couldn't cut");
}

#[test]
fn n3_sync_eio_with_clean_rollback_is_not_committed_but_poisoned() {
    let (_d, mut log, disk, committed) = rigged(Some(FailMode::SyncEioOnce), b"");
    let err = log.append_all(&[rec(1, "b")]).unwrap_err().to_string();
    assert!(err.contains("not committed (writer poisoned)"), "{err}");
    assert!(log.is_poisoned(), "failed sync must poison even when rolled back");
    assert_eq!(disk_bytes(&disk), committed);
}

#[test]
fn n4_sync_eio_with_failed_rollback_is_unknown_and_poisoned() {
    // The batch is fully on "disk" and closed, yet unconfirmed: after a
    // restart the scan would accept it. That is why the outcome is unknown.
    let (_d, mut log, disk, _) = rigged(Some(FailMode::SyncEioAndTruncateEperm), b"");
    let err = log.append_all(&[rec(1, "b")]).unwrap_err().to_string();
    assert!(err.contains("commit outcome unknown (writer poisoned)"), "{err}");
    assert!(log.is_poisoned());
    assert_eq!(scan(&disk_bytes(&disk)).unwrap().len, 2);
}

#[test]
fn n5_shrink_under_writer_is_corruption_and_poisons() {
    let (_d, mut log, disk, committed) = rigged(Some(FailMode::ShrunkTo(0)), b"");
    let err = log.append_all(&[rec(1, "b")]).unwrap_err().to_string();
    assert!(err.contains("shrank under writer"), "{err}");
    assert!(err.contains("data corruption"), "{err}");
    assert!(log.is_poisoned());
    assert_eq!(disk_bytes(&disk), committed, "must not touch a file it can't vouch for");
}

#[test]
fn n6_poison_sticks_until_reopen() {
    let (dir, mut log, disk, _) = rigged(Some(FailMode::SyncEio), b"");
    let _ = log.append_all(&[rec(1, "b")]).unwrap_err();
    assert!(log.is_poisoned());
    let frozen = disk_bytes(&disk);
    for i in 1..4 {
        let err = log.append_all(&[rec(i, "x")]).unwrap_err().to_string();
        assert!(err.contains("poisoned"), "{err}");
    }
    assert_eq!(disk_bytes(&disk), frozen, "a poisoned writer must touch nothing");
    // Reopen re-derives the truth from the real bytes: healthy again.
    let (log2, found) = Log::open_sealed(dir.path().join("log.jsonl")).unwrap();
    assert_eq!(found.len, 1);
    assert!(!log2.is_poisoned());
}

#[test]
fn n7_live_writer_cuts_residue_before_the_next_batch() {
    // Residue appears under a live writer (not at open): the writer cuts
    // it, then the batch lands contiguous right after the committed end.
    let (_d, mut log, disk, committed) = rigged(None, b"{\"seq\":1,\"ts_ms\":17");
    log.append_all(&[rec(1, "b")]).unwrap();
    let bytes = disk_bytes(&disk);
    assert!(bytes.starts_with(&committed));
    assert!(bytes[committed.len()..].starts_with(b"{\"seq\":1,\"ts_ms\":1001,"), "batch not contiguous");
    let found = scan(&bytes).unwrap();
    assert_eq!((found.len, found.end), (2, bytes.len()));
}
