//! `rewrite`: publication, refusals, corrupt input, byte identity, and the
//! fault-injection matrix over every checkpoint a rewrite reaches.

mod common;

use std::path::{Path, PathBuf};

use common::*;
use durable::census;
use durable::{dry_run, rewrite, Emit, Error, Mode, Result, RewriteReport, Step};

fn keep<E>(_s: Step<'_, E>) -> Result<Emit<E>> {
    Ok(Emit::Keep)
}

fn ext_batches() -> Vec<Vec<(u64, Ext)>> {
    vec![
        vec![(
            100,
            Ext::Add {
                key: "a".into(),
                n: 3,
            },
        )],
        vec![
            (101, Ext::Note("héllo \"wörld\"\n\u{1F600}".into())),
            (
                102,
                Ext::Pair {
                    a: u64::MAX,
                    b: "z".into(),
                },
            ),
        ],
        vec![
            (103, Ext::Reset),
            (
                104,
                Ext::Add {
                    key: "a".into(),
                    n: 1,
                },
            ),
            (105, Ext::Reset),
        ],
    ]
}

/// Source tape in `dir`; returns (src path, its bytes).
fn source(dir: &Path, batches: &[Vec<(u64, Ext)>]) -> (PathBuf, Vec<u8>) {
    let src = dir.join("ladder.jsonl");
    write_lines(&src, &tape_lines(batches));
    let bytes = read_bytes(&src);
    (src, bytes)
}

/// Assert a refused rewrite left the world as it was: no destination, the
/// source byte-identical, and no temp file in the directory.
fn assert_untouched(dir: &Path, src: &Path, src_bytes: &[u8], dst: &Path, names_before: &[String]) {
    assert!(!dst.exists(), "destination must not exist after a refusal");
    assert_eq!(read_bytes(src), src_bytes, "source must be byte-identical");
    assert_eq!(
        dir_names(dir),
        names_before,
        "no temp files may be left behind"
    );
}

#[test]
fn keep_everywhere_is_byte_identical_including_crcs() {
    let dir = tmp_dir("rw-identity");
    let (src, bytes) = source(&dir, &ext_batches());
    let dst = dir.join("out.jsonl");
    let report =
        rewrite::<Ext, Ext>(&src, &dst, schema(), reduce_ext, Mode::SameSeq, keep).unwrap();
    assert_eq!(read_bytes(&dst), bytes);
    assert_eq!(report.read, 6);
    assert_eq!(report.written, 6);
    assert_eq!(report.replaced, 0);
    assert_eq!(report.dropped, 0);
    assert_eq!(report.batches_written, 3);
    assert_eq!(report.src_committed_bytes, bytes.len() as u64);
    assert!(!report.dry_run);
    // The temp was published by link and removed: only src and dst remain.
    assert_eq!(
        dir_names(&dir),
        vec!["ladder.jsonl".to_string(), "out.jsonl".to_string()]
    );
}

#[test]
fn replace_keeps_seq_ts_and_batches() {
    let dir = tmp_dir("rw-replace");
    let (src, _) = source(&dir, &ext_batches());
    let dst = dir.join("out.jsonl");
    let report = rewrite::<Ext, Ext>(&src, &dst, schema(), reduce_ext, Mode::SameSeq, |s| {
        Ok(match s.rec.event {
            Ext::Note(_) => Emit::Replace(Ext::Note("replaced".into())),
            _ => Emit::Keep,
        })
    })
    .unwrap();
    assert_eq!(report.replaced, 1);
    let c = census::census(&dst, "type").unwrap();
    assert_eq!(c.records, 6);
    assert_eq!(c.batches, 3);
    assert_eq!(c.batch_sizes.get(&1), Some(&1));
    assert_eq!(c.batch_sizes.get(&2), Some(&1));
    assert_eq!(c.batch_sizes.get(&3), Some(&1));
    assert_eq!(c.ts_min, Some(100));
    assert_eq!(c.ts_max, Some(105));
    assert_eq!(census::verify(&dst).unwrap().tail_bytes, 0);
    let lines = census::cat(&dst, 0).unwrap();
    let text = String::from_utf8(lines[1].clone()).unwrap();
    assert!(text.contains("\"replaced\""), "{text}");
    assert!(
        text.starts_with("{\"seq\":1,\"ts_ms\":101,\"from\":1,\"to\":2,"),
        "{text}"
    );
}

#[test]
fn sameseq_refuses_drop_and_writes_nothing() {
    let dir = tmp_dir("rw-drop-sameseq");
    let (src, bytes) = source(&dir, &ext_batches());
    let before = dir_names(&dir);
    let dst = dir.join("out.jsonl");
    let err = rewrite::<Ext, Ext>(&src, &dst, schema(), reduce_ext, Mode::SameSeq, |s| {
        Ok(if s.rec.seq == 2 {
            Emit::Drop
        } else {
            Emit::Keep
        })
    })
    .unwrap_err();
    assert!(err.to_string().contains("SameSeq"), "{err}");
    assert_untouched(&dir, &src, &bytes, &dst, &before);
}

#[test]
fn map_error_leaves_destination_absent() {
    let dir = tmp_dir("rw-map-err");
    let (src, bytes) = source(&dir, &ext_batches());
    let before = dir_names(&dir);
    let dst = dir.join("out.jsonl");
    let err = rewrite::<Ext, Ext>(&src, &dst, schema(), reduce_ext, Mode::Renumber, |s| {
        if s.rec.seq == 4 {
            Err(Error::Reducer("refuse seq 4".into()))
        } else {
            Ok(Emit::Keep)
        }
    })
    .unwrap_err();
    assert!(err.to_string().contains("refuse seq 4"));
    assert_untouched(&dir, &src, &bytes, &dst, &before);
}

#[test]
fn existing_destination_is_refused_and_not_overwritten() {
    let dir = tmp_dir("rw-exists");
    let (src, bytes) = source(&dir, &ext_batches());
    let dst = dir.join("out.jsonl");
    std::fs::write(&dst, b"precious").unwrap();
    let before = dir_names(&dir);
    let err =
        rewrite::<Ext, Ext>(&src, &dst, schema(), reduce_ext, Mode::SameSeq, keep).unwrap_err();
    assert!(err.to_string().contains("already exists"), "{err}");
    assert_eq!(read_bytes(&dst), b"precious");
    assert_eq!(read_bytes(&src), bytes);
    assert_eq!(dir_names(&dir), before);
}

#[test]
fn destination_directory_must_exist() {
    let dir = tmp_dir("rw-nodir");
    let (src, _) = source(&dir, &ext_batches());
    let dst = dir.join("missing").join("out.jsonl");
    let err =
        rewrite::<Ext, Ext>(&src, &dst, schema(), reduce_ext, Mode::SameSeq, keep).unwrap_err();
    assert!(err.to_string().contains("does not exist"), "{err}");
}

#[test]
fn event_that_does_not_decode_as_the_old_enum_is_refused() {
    let dir = tmp_dir("rw-decode");
    let src = dir.join("int.jsonl");
    // `Int` has no `Bogus` variant: the old enum cannot read this record.
    let lines = vec![
        sealed_raw(0, 1, 0, 0, r#"{"type":"add","key":"a","n":1}"#),
        sealed_raw(1, 2, 1, 1, r#"{"type":"bogus","x":1}"#),
    ];
    write_lines(&src, &lines);
    let bytes = read_bytes(&src);
    let before = dir_names(&dir);
    let dst = dir.join("out.jsonl");
    let err =
        rewrite::<Int, Int>(&src, &dst, schema(), reduce_int, Mode::SameSeq, keep).unwrap_err();
    assert!(
        matches!(err, Error::Deserialize(ref m) if m.contains("seq 1")),
        "{err:?}"
    );
    assert_untouched(&dir, &src, &bytes, &dst, &before);
}

#[test]
fn renumber_drops_renumbers_and_keeps_batches_valid() {
    let dir = tmp_dir("rw-renumber");
    let (src, bytes) = source(&dir, &ext_batches());
    let dst = dir.join("out.jsonl");
    // Drop odd seqs: batch [0..0] loses nothing (seq 0 kept), [1..2] keeps 2,
    // [3..5] keeps 4.
    let report = rewrite::<Ext, Ext>(&src, &dst, schema(), reduce_ext, Mode::Renumber, |s| {
        Ok(if s.rec.seq % 2 == 1 {
            Emit::Drop
        } else {
            Emit::Keep
        })
    })
    .unwrap();
    assert_eq!(report.read, 6);
    assert_eq!(report.dropped, 3);
    assert_eq!(report.written, 3);
    let lines = census::cat(&dst, 0).unwrap();
    assert_eq!(lines.len(), 3);
    // Output seqs are 0,1,2 and the batches are [0],[1],[2].
    for (i, line) in lines.iter().enumerate() {
        let text = String::from_utf8(line.clone()).unwrap();
        assert!(text.starts_with(&format!("{{\"seq\":{i},")), "{text}");
        assert!(
            text.contains(&format!("\"from\":{i},\"to\":{i},")),
            "{text}"
        );
    }
    assert_eq!(census::verify(&dst).unwrap().records, 3);
    // Kept event bytes are the source's: the u64::MAX payload survives exactly.
    let pair = String::from_utf8(lines[1].clone()).unwrap();
    assert!(pair.contains("18446744073709551615"), "{pair}");
    assert_ne!(
        read_bytes(&dst),
        bytes,
        "renumbering must change the output"
    );
}

#[test]
fn renumber_copies_unchanged_lines_verbatim() {
    let dir = tmp_dir("rw-verbatim");
    let (src, bytes) = source(&dir, &ext_batches());
    let dst = dir.join("out.jsonl");
    // Drop nothing: every seq/from/to is unchanged, so output is the input.
    rewrite::<Ext, Ext>(&src, &dst, schema(), reduce_ext, Mode::Renumber, keep).unwrap();
    assert_eq!(read_bytes(&dst), bytes);
}

#[test]
fn renumber_reseals_lines_when_bounds_shift_but_keeps_event_bytes() {
    let dir = tmp_dir("rw-reseal");
    // One batch [0..1] with a non-canonical event; drop seq 0, so seq 1 becomes
    // seq 0 with batch [0..0]. The event text must survive byte for byte.
    let src = dir.join("raw.jsonl");
    let raw = r#"{ "type" : "note",   "text":"a  b\tc" }"#;
    let lines = vec![
        sealed_raw(0, 10, 0, 1, r#"{"type":"reset"}"#),
        sealed_raw(1, 11, 0, 1, raw),
    ];
    write_lines(&src, &lines);
    let dst = dir.join("out.jsonl");
    rewrite::<Int, Int>(&src, &dst, schema(), reduce_int, Mode::Renumber, |s| {
        Ok(if s.rec.seq == 0 {
            Emit::Drop
        } else {
            Emit::Keep
        })
    })
    .unwrap();
    let out = census::cat(&dst, 0).unwrap();
    assert_eq!(out.len(), 1);
    let text = String::from_utf8(out[0].clone()).unwrap();
    assert!(
        text.starts_with(
            r#"{"seq":0,"ts_ms":11,"from":0,"to":0,"event":{ "type" : "note",   "text":"a  b\tc" }"#
        ),
        "{text}"
    );
    assert_eq!(census::verify(&dst).unwrap().records, 1);
}

#[test]
fn dry_run_writes_nothing_and_reports() {
    let dir = tmp_dir("rw-dry");
    let (src, bytes) = source(&dir, &ext_batches());
    let before = dir_names(&dir);
    let report = dry_run::<Ext, Ext>(&src, schema(), reduce_ext, Mode::Renumber, |s| {
        Ok(if matches!(s.rec.event, Ext::Reset) {
            Emit::Drop
        } else {
            Emit::Keep
        })
    })
    .unwrap();
    assert!(report.dry_run);
    assert_eq!(report.dropped, 2);
    assert_eq!(report.written, 4);
    assert_eq!(report.dst_bytes, 0);
    assert_eq!(dir_names(&dir), before);
    assert_eq!(read_bytes(&src), bytes);
    let text = report.to_text();
    assert!(
        text.starts_with("mode: dry-run\nread: 6\nwritten: 4\n"),
        "{text}"
    );
    assert!(text.contains("dropped: 2\n"), "{text}");
}

#[test]
fn report_text_is_stable_key_value_lines() {
    let r = RewriteReport {
        read: 9,
        written: 7,
        replaced: 1,
        dropped: 2,
        batches_written: 3,
        src_committed_bytes: 100,
        dst_bytes: 90,
        dst_dir_synced: true,
        dry_run: false,
    };
    assert_eq!(
        r.to_text(),
        "mode: rewrite\nread: 9\nwritten: 7\nreplaced: 1\ndropped: 2\nbatches_written: 3\n\
         src_committed_bytes: 100\ndst_bytes: 90\ndst_dir_synced: true\n"
    );
}

// ---- corrupt input: every case refuses, writes nothing, touches nothing ----

fn expect_refusal(name: &str, lines_or_bytes: Vec<u8>) {
    let dir = tmp_dir(&format!("rw-corrupt-{name}"));
    let src = dir.join("tape.jsonl");
    std::fs::write(&src, &lines_or_bytes).unwrap();
    let before = dir_names(&dir);
    let dst = dir.join("out.jsonl");
    let err =
        rewrite::<Int, Int>(&src, &dst, schema(), reduce_int, Mode::SameSeq, keep).expect_err(name);
    assert!(
        matches!(
            err,
            Error::Corruption(_) | Error::Deserialize(_) | Error::Log(_)
        ),
        "{name}: {err:?}"
    );
    assert_untouched(&dir, &src, &lines_or_bytes, &dst, &before);
}

fn good_lines() -> Vec<String> {
    vec![
        sealed_raw(0, 1, 0, 0, r#"{"type":"add","key":"a","n":1}"#),
        sealed_raw(1, 2, 1, 2, r#"{"type":"note","text":"x"}"#),
        sealed_raw(2, 3, 1, 2, r#"{"type":"reset"}"#),
    ]
}

fn joined(lines: &[String]) -> Vec<u8> {
    let mut out = String::new();
    for l in lines {
        out.push_str(l);
        out.push('\n');
    }
    out.into_bytes()
}

#[test]
fn corrupt_bad_crc_in_committed_prefix_is_refused() {
    let mut lines = good_lines();
    // Flip one CRC digit on the first line; later batches survive, so this is
    // damage to acknowledged bytes.
    let l = lines[0].clone();
    let mut bytes = l.into_bytes();
    let n = bytes.len();
    bytes[n - 4] = if bytes[n - 4] == b'0' { b'1' } else { b'0' };
    lines[0] = String::from_utf8(bytes).unwrap();
    expect_refusal("bad-crc", joined(&lines));
}

#[test]
fn corrupt_duplicate_seq_is_refused() {
    let mut lines = good_lines();
    lines.insert(1, lines[0].clone());
    expect_refusal("duplicate-seq", joined(&lines));
}

#[test]
fn corrupt_gapped_seq_is_refused() {
    let lines = vec![
        sealed_raw(0, 1, 0, 0, r#"{"type":"reset"}"#),
        sealed_raw(2, 2, 2, 2, r#"{"type":"reset"}"#),
    ];
    expect_refusal("gapped-seq", joined(&lines));
}

#[test]
fn corrupt_seq_outside_its_batch_is_refused() {
    let lines = vec![
        sealed_raw(0, 1, 0, 0, r#"{"type":"reset"}"#),
        sealed_raw(5, 2, 1, 2, r#"{"type":"reset"}"#),
        sealed_raw(2, 3, 1, 2, r#"{"type":"reset"}"#),
    ];
    expect_refusal("seq-outside-batch", joined(&lines));
}

#[test]
fn corrupt_overlong_line_is_refused() {
    let big = format!(
        r#"{{"type":"note","text":"{}"}}"#,
        "x".repeat(durable::runtime::MAX_SEALED_LINE)
    );
    let mut bytes = joined(&[sealed_raw(0, 1, 0, 0, r#"{"type":"reset"}"#)]);
    bytes.extend_from_slice(joined(&[sealed_raw(1, 2, 1, 1, &big)]).as_slice());
    bytes.extend_from_slice(joined(&[sealed_raw(2, 3, 2, 2, r#"{"type":"reset"}"#)]).as_slice());
    expect_refusal("overlong", bytes);
}

#[test]
fn corrupt_invalid_utf8_is_refused() {
    let mut bytes = joined(&good_lines()[..1]);
    bytes.extend_from_slice(b"\xff\xfe not json at all\n");
    bytes.extend_from_slice(joined(&good_lines()[1..]).as_slice());
    expect_refusal("invalid-utf8", bytes);
}

#[test]
fn corrupt_extra_envelope_key_with_valid_crc_is_refused() {
    // A newer writer's envelope: `v` is CRC-covered, so the line is authentic
    // and this build must refuse it, not drop it as residue.
    let prefix = r#"{"seq":1,"ts_ms":2,"from":1,"to":2,"v":2,"event":{"type":"reset"}"#;
    let crc = durable::runtime::seal_crc(prefix.as_bytes());
    let extra = format!("{prefix},\"crc\":\"{crc}\"}}");
    let lines = vec![good_lines()[0].clone(), extra, good_lines()[2].clone()];
    expect_refusal("extra-envelope-key", joined(&lines));
}

#[test]
fn corrupt_extra_envelope_key_in_final_batch_is_refused_not_residue() {
    let prefix = r#"{"seq":1,"ts_ms":2,"from":1,"to":1,"v":2,"event":{"type":"reset"}"#;
    let crc = durable::runtime::seal_crc(prefix.as_bytes());
    let extra = format!("{prefix},\"crc\":\"{crc}\"}}");
    let lines = vec![good_lines()[0].clone(), extra];
    expect_refusal("extra-key-tail", joined(&lines));
}

#[test]
fn corrupt_unsealed_json_line_is_refused() {
    let mut bytes = joined(&good_lines()[..1]);
    bytes.extend_from_slice(br#"{"seq":1,"ts_ms":2,"event":{"type":"reset"}}"#);
    bytes.push(b'\n');
    bytes.extend_from_slice(joined(&good_lines()[1..]).as_slice());
    expect_refusal("unsealed", bytes);
}

#[test]
fn corrupt_torn_tail_is_refused_by_rewrite() {
    let mut bytes = joined(&good_lines());
    bytes.extend_from_slice(br#"{"seq":3,"ts_ms":4,"from":3,"to":3,"event":{"ty"#);
    expect_refusal("torn-tail", bytes);
}

#[test]
fn corrupt_damaged_last_line_with_newline_is_refused_as_tail() {
    let mut lines = good_lines();
    let mut bytes = lines.remove(2).into_bytes();
    let n = bytes.len();
    bytes[n - 3] = b'9';
    let mut all = joined(&lines);
    all.extend_from_slice(&bytes);
    all.push(b'\n');
    expect_refusal("damaged-final", all);
}

// ---- fault injection over every checkpoint a rewrite reaches ----

#[cfg(feature = "fault-injection")]
#[test]
fn fault_matrix_rewrite_every_checkpoint() {
    use durable::fault;

    let dir = tmp_dir("rw-faults");
    let (src, bytes) = source(&dir, &ext_batches());
    let reference = dir.join("reference.jsonl");

    // Count the checkpoints of one clean run.
    fault::arm(u64::MAX);
    rewrite::<Ext, Ext>(
        &src,
        &reference,
        schema(),
        reduce_ext,
        Mode::Renumber,
        |s| {
            Ok(if s.rec.seq == 3 {
                Emit::Drop
            } else {
                Emit::Keep
            })
        },
    )
    .unwrap();
    let labels = fault::labels();
    fault::disarm();
    assert!(
        labels.len() >= 6,
        "expected several checkpoints, saw {labels:?}"
    );
    let reference_bytes = read_bytes(&reference);
    std::fs::remove_file(&reference).unwrap();

    let names_before = dir_names(&dir);
    let mut exercised = Vec::new();
    for n in 0..labels.len() as u64 {
        let dst = dir.join(format!("out-{n}.jsonl"));
        fault::arm(n);
        let result = rewrite::<Ext, Ext>(&src, &dst, schema(), reduce_ext, Mode::Renumber, |s| {
            Ok(if s.rec.seq == 3 {
                Emit::Drop
            } else {
                Emit::Keep
            })
        });
        let hit = fault::disarm();
        assert!(hit > n, "checkpoint {n} was not reached");
        let label = labels[n as usize];
        exercised.push(label);
        assert!(
            result.is_err(),
            "fault at {n} ({label}) must fail the rewrite"
        );
        assert!(!dst.exists(), "fault at {n} ({label}) left a destination");
        assert_eq!(
            read_bytes(&src),
            bytes,
            "fault at {n} ({label}) changed the source"
        );
        assert_eq!(
            dir_names(&dir),
            names_before,
            "fault at {n} ({label}) left a temp file"
        );

        // The retry, with no fault armed, succeeds and yields the reference.
        rewrite::<Ext, Ext>(&src, &dst, schema(), reduce_ext, Mode::Renumber, |s| {
            Ok(if s.rec.seq == 3 {
                Emit::Drop
            } else {
                Emit::Keep
            })
        })
        .unwrap_or_else(|e| panic!("retry after {label} failed: {e}"));
        assert_eq!(
            read_bytes(&dst),
            reference_bytes,
            "retry after {label} differs"
        );
        std::fs::remove_file(&dst).unwrap();
    }
    // Every write, fsync, link and verify boundary was exercised.
    for needed in [
        "rewrite.create_temp",
        "rewrite.write",
        "rewrite.publish.sync",
        "rewrite.publish.link",
        "rewrite.verify",
    ] {
        assert!(
            exercised.contains(&needed),
            "no fault injected at {needed}: {exercised:?}"
        );
    }
    println!(
        "rewrite fault matrix: {} checkpoints exercised: {:?}",
        exercised.len(),
        exercised
    );
}

/// kill -9 at a random byte offset of the output: the truncated file is either
/// a clean shorter tape (cut on a batch boundary) or a tape with a torn tail.
/// `verify` must say which; `rewrite` must refuse the torn one; neither may
/// invent records.
#[test]
fn truncated_output_at_random_offsets_is_detected() {
    use proptest::prelude::*;
    let dir = tmp_dir("rw-trunc");
    let (src, _) = source(&dir, &ext_batches());
    let full = dir.join("full.jsonl");
    rewrite::<Ext, Ext>(&src, &full, schema(), reduce_ext, Mode::SameSeq, keep).unwrap();
    let bytes = read_bytes(&full);
    let runner = &mut proptest::test_runner::TestRunner::new(ProptestConfig::with_cases(64));
    runner
        .run(&(0usize..=bytes.len()), |cut| {
            let cut_path = dir.join("cut.jsonl");
            std::fs::write(&cut_path, &bytes[..cut]).unwrap();
            let found = census::verify(&cut_path).unwrap();
            let boundary = cut == 0
                || bytes[cut - 1] == b'\n' && {
                    // Boundary iff the committed end equals the cut.
                    found.committed_end as usize == cut
                };
            if boundary {
                prop_assert_eq!(found.tail_bytes, 0);
            } else {
                prop_assert!(found.tail_bytes > 0, "cut {cut} not detected");
                let out = dir.join("cut-out.jsonl");
                let err =
                    rewrite::<Ext, Ext>(&cut_path, &out, schema(), reduce_ext, Mode::SameSeq, keep)
                        .expect_err("torn tail must be refused");
                prop_assert!(matches!(err, Error::Corruption(_)), "{err:?}");
                prop_assert!(!out.exists());
            }
            prop_assert!(found.records as usize <= 6);
            Ok(())
        })
        .unwrap();
}

#[test]
fn rewrite_output_is_fsynced_and_rescanned_before_publication() {
    // Structural: the report says the parent directory sync result; on a
    // normal filesystem it succeeds.
    let dir = tmp_dir("rw-report");
    let (src, _) = source(&dir, &ext_batches());
    let dst = dir.join("out.jsonl");
    let report =
        rewrite::<Ext, Ext>(&src, &dst, schema(), reduce_ext, Mode::SameSeq, keep).unwrap();
    assert!(report.dst_dir_synced);
    assert_eq!(report.dst_bytes, read_bytes(&dst).len() as u64);
}
