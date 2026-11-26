//! Property tests for `rewrite`: byte identity with Keep, equivalence under an
//! injective Replace, Renumber with random drops, determinism, composition.
//! Event streams mix the three serde-tagging fixtures, unicode and escapes,
//! non-canonical JSON spacing, batches of random sizes, and ts patterns.

mod common;

use std::path::Path;

use common::*;
use durable::testing::{app_keys_only, assert_rewrite_equivalent, check_rewrite_equivalent};
use durable::{census, digest_pairs, fold_dump, rewrite, Emit, Mode, Result, Step};
use proptest::prelude::*;

// ---------- generators ----------

fn text() -> impl Strategy<Value = String> {
    prop_oneof![
        "[\\PC]{0,12}",
        Just(String::new()),
        Just("quote \" and back\\slash".to_string()),
        Just("tab\tnew\nline ".to_string()),
        Just("  spaces   inside  ".to_string()),
        Just("emoji \u{1F600} and \u{2028} separator".to_string()),
        Just("héllo wörld ñ 中文".to_string()),
        Just("}, ] { [ : ,".to_string()),
    ]
}

fn key() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("a".to_string()),
        Just("b c".to_string()),
        "[a-z]{1,4}".prop_map(|s| s),
        Just("é".to_string()),
    ]
}

fn ext_event() -> BoxedStrategy<Ext> {
    prop_oneof![
        (key(), any::<u64>()).prop_map(|(key, n)| Ext::Add { key, n: n % 1000 }),
        text().prop_map(Ext::Note),
        Just(Ext::Reset),
        (any::<u64>(), key()).prop_map(|(a, b)| Ext::Pair { a, b }),
    ]
    .boxed()
}

fn int_event() -> BoxedStrategy<Int> {
    prop_oneof![
        (key(), any::<u64>()).prop_map(|(key, n)| Int::Add { key, n: n % 1000 }),
        text().prop_map(|text| Int::Note { text }),
        Just(Int::Reset),
        (key(), any::<u64>()).prop_map(|(key, n)| Int::Wrap(Pay { key, n: n % 1000 })),
    ]
    .boxed()
}

/// Map an externally tagged event to its internally tagged twin: the same
/// meaning, a different variant and tagging. Injective and total.
fn ext_to_int(e: &Ext) -> Int {
    match e {
        Ext::Add { key, n } => Int::Add {
            key: key.clone(),
            n: *n,
        },
        Ext::Note(s) => Int::Note { text: s.clone() },
        Ext::Reset => Int::Reset,
        Ext::Pair { a, b } => Int::Wrap(Pay {
            key: b.clone(),
            n: *a,
        }),
    }
}

/// Insert spaces after `:` and `,` outside JSON strings: valid JSON, different
/// bytes. Exercises byte-identical copying of non-canonical event text.
fn spaced(json: &str) -> String {
    let mut out = String::with_capacity(json.len() * 2);
    let mut in_str = false;
    let mut escaped = false;
    for ch in json.chars() {
        out.push(ch);
        if in_str {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_str = false;
            }
        } else if ch == '"' {
            in_str = true;
        } else if ch == ':' || ch == ',' {
            out.push_str("  ");
        }
    }
    out
}

fn ts_pattern(n: usize, pattern: u8, base: u64) -> Vec<u64> {
    (0..n as u64)
        .map(|i| match pattern % 4 {
            0 => base + i,                  // monotone, dense
            1 => base,                      // constant
            2 => base + i * 1_000_003,      // sparse
            _ => u64::MAX - (n as u64) + i, // huge
        })
        .collect()
}

/// Batches of random sizes, random ts pattern, and per-line raw/canonical choice.
#[derive(Debug, Clone)]
struct Tape<E> {
    batches: Vec<Vec<(u64, E)>>,
    raw: Vec<Vec<bool>>,
}

fn tape_strategy<E: std::fmt::Debug + Clone + 'static>(
    event: BoxedStrategy<E>,
) -> impl Strategy<Value = Tape<E>> {
    (
        proptest::collection::vec(1usize..=6, 1..=6),
        0u8..4,
        any::<u64>(),
        proptest::collection::vec(any::<bool>(), 64),
        proptest::collection::vec(event, 40),
    )
        .prop_map(|(sizes, pattern, base, raw_flags, events)| {
            let total: usize = sizes.iter().sum();
            let tss = ts_pattern(total, pattern, base % 1_000_000);
            let mut batches = Vec::new();
            let mut raw = Vec::new();
            let mut idx = 0usize;
            for size in sizes {
                let mut batch = Vec::new();
                let mut flags = Vec::new();
                for _ in 0..size {
                    batch.push((tss[idx], events[idx % events.len()].clone()));
                    flags.push(raw_flags[idx % raw_flags.len()]);
                    idx += 1;
                }
                batches.push(batch);
                raw.push(flags);
            }
            Tape { batches, raw }
        })
}

/// Lines for a tape: canonical or spaced, per the per-line flag.
fn lines_of<E: serde::Serialize>(tape: &Tape<E>) -> Vec<String> {
    let mut lines = Vec::new();
    let mut seq = 0u64;
    for (batch, flags) in tape.batches.iter().zip(&tape.raw) {
        let from = seq;
        let to = seq + batch.len() as u64 - 1;
        for ((ts, ev), raw) in batch.iter().zip(flags) {
            let canonical = serde_json::to_string(ev).unwrap();
            let body = if *raw { spaced(&canonical) } else { canonical };
            lines.push(sealed_raw(seq, *ts, from, to, &body));
            seq += 1;
        }
    }
    lines
}

fn write_tape<E: serde::Serialize>(path: &Path, tape: &Tape<E>) {
    write_lines(path, &lines_of(tape));
}

fn keep<E>(_s: Step<'_, E>) -> Result<Emit<E>> {
    Ok(Emit::Keep)
}

fn is_real_clean(dir: &Path) {
    let names = dir_names(dir);
    assert!(
        names.iter().all(|n| !n.contains(".tmp-")),
        "temp left: {names:?}"
    );
}

// ---------- properties ----------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(96))]

    /// (a) Keep everywhere is byte-identical to the source, CRCs included,
    /// for every mix of tagging, spacing, ts pattern and batch size.
    #[test]
    fn keep_everywhere_is_byte_identical(tape in tape_strategy(ext_event())) {
        let dir = tmp_dir("prop-identity");
        let src = dir.join("src.jsonl");
        write_tape(&src, &tape);
        let bytes = read_bytes(&src);
        let dst = dir.join("dst.jsonl");
        rewrite::<Totals, Ext, Ext>(&src, &dst, reduce_ext, Mode::SameSeq, keep).unwrap();
        prop_assert_eq!(read_bytes(&dst), bytes);
        is_real_clean(&dir);
    }

    /// (a') The same for internally tagged events, whose raw text is spaced.
    #[test]
    fn keep_everywhere_is_byte_identical_internally_tagged(tape in tape_strategy(int_event())) {
        let dir = tmp_dir("prop-identity-int");
        let src = dir.join("src.jsonl");
        write_tape(&src, &tape);
        let bytes = read_bytes(&src);
        let dst = dir.join("dst.jsonl");
        rewrite::<Totals, Int, Int>(&src, &dst, reduce_int, Mode::SameSeq, keep).unwrap();
        prop_assert_eq!(read_bytes(&dst), bytes);
    }

    /// (b) Replacing every event by its injective twin, then folding the new
    /// tape with the twin reducer, equals folding the old tape with the old one.
    #[test]
    fn injective_replace_is_projection_equivalent(tape in tape_strategy(ext_event())) {
        let dir = tmp_dir("prop-equiv");
        let src = dir.join("old.jsonl");
        write_tape(&src, &tape);
        let dst = dir.join("new.jsonl");
        rewrite::<Totals, Ext, Int>(&src, &dst, reduce_ext, Mode::SameSeq, |s| {
            Ok(Emit::Replace(ext_to_int(&s.rec.event)))
        })
        .unwrap();
        assert_rewrite_equivalent::<Totals, Ext, Int>(
            &src,
            &dst,
            reduce_ext,
            reduce_int,
            app_keys_only,
        );
    }

    /// (b') Control: a Replace that changes meaning must be detected, and the
    /// diff must name a key and both values.
    #[test]
    fn meaning_change_is_detected_with_a_readable_diff(tape in tape_strategy(ext_event())) {
        let has_add = tape.batches.iter().flatten().any(|(_, e)| matches!(e, Ext::Add { .. }));
        prop_assume!(has_add);
        let dir = tmp_dir("prop-diff");
        let src = dir.join("old.jsonl");
        write_tape(&src, &tape);
        let dst = dir.join("new.jsonl");
        rewrite::<Totals, Ext, Ext>(&src, &dst, reduce_ext, Mode::SameSeq, |s| {
            Ok(match &s.rec.event {
                Ext::Add { key, n } => Emit::Replace(Ext::Add { key: key.clone(), n: n + 1 }),
                _ => Emit::Keep,
            })
        })
        .unwrap();
        let diff = check_rewrite_equivalent::<Totals, Ext, Ext>(
            &src, &dst, reduce_ext, reduce_ext, app_keys_only,
        );
        let diff = diff.expect_err("a changed amount must not compare equal");
        prop_assert!(diff.contains("first differing key"), "{diff}");
        prop_assert!(diff.contains("old:") && diff.contains("new:"), "{diff}");
    }

    /// (c) Renumber with random drops: output seqs are dense from 0, every
    /// batch's from/to is consistent and verify passes, and the output's fold
    /// equals the fold of the filtered stream written fresh.
    #[test]
    fn renumber_with_drops_is_consistent_and_equals_filtered_fold(
        tape in tape_strategy(ext_event()),
        mask in proptest::collection::vec(any::<bool>(), 40),
    ) {
        let dir = tmp_dir("prop-renumber");
        let src = dir.join("src.jsonl");
        write_tape(&src, &tape);
        let dst = dir.join("dst.jsonl");
        let mut counter = 0usize;
        // Drop by mask over the source seq; the mask is indexed by seq.
        let report = rewrite::<Totals, Ext, Ext>(&src, &dst, reduce_ext, Mode::Renumber, |s| {
            let drop = mask[s.rec.seq as usize % mask.len()];
            counter += 1;
            Ok(if drop { Emit::Drop } else { Emit::Keep })
        })
        .unwrap();
        let total: usize = tape.batches.iter().map(|b| b.len()).sum();
        prop_assert_eq!(report.read as usize, total);
        // Expected: the kept events, grouped by their source batch.
        let mut expected_batches: Vec<Vec<(u64, Ext)>> = Vec::new();
        let mut seq = 0usize;
        for batch in &tape.batches {
            let kept: Vec<(u64, Ext)> = batch
                .iter()
                .enumerate()
                .filter(|(i, _)| !mask[(seq + i) % mask.len()])
                .map(|(_, r)| r.clone())
                .collect();
            seq += batch.len();
            if !kept.is_empty() {
                expected_batches.push(kept);
            }
        }
        let kept_total: usize = expected_batches.iter().map(|b| b.len()).sum();
        prop_assert_eq!(report.written as usize, kept_total);
        prop_assert_eq!(report.dropped as usize, total - kept_total);
        prop_assert_eq!(counter, total);

        // Invariants on the output lines.
        let out = census::cat(&dst, 0).unwrap();
        prop_assert_eq!(out.len(), kept_total);
        let mut i = 0usize;
        while i < out.len() {
            let text = String::from_utf8(out[i].clone()).unwrap();
            let (seq_i, from, to) = parse_env(&text);
            prop_assert_eq!(seq_i, i as u64);
            prop_assert!(from <= seq_i && seq_i <= to);
            // Walk the batch: every member shares from/to; members are dense.
            let mut j = i;
            while j < out.len() {
                let t = String::from_utf8(out[j].clone()).unwrap();
                let (s, f, t_to) = parse_env(&t);
                prop_assert_eq!(s, j as u64);
                prop_assert_eq!((f, t_to), (from, to));
                if s == to { j += 1; break; }
                j += 1;
            }
            prop_assert_eq!(j - i, (to - from + 1) as usize);
            i = j;
        }
        prop_assert!(census::verify(&dst).is_ok());

        // Equivalence with the filtered stream written fresh.
        let fresh = dir.join("fresh.jsonl");
        write_lines(&fresh, &tape_lines(&expected_batches));
        let got = fold_dump::<Totals, Ext>(&dst, reduce_ext).unwrap();
        let want = fold_dump::<Totals, Ext>(&fresh, reduce_ext).unwrap();
        prop_assert_eq!(app_only(got), app_only(want));
    }

    /// (d) Determinism: the same input and rule give identical bytes and digests.
    #[test]
    fn same_inputs_give_identical_outputs(tape in tape_strategy(ext_event())) {
        let dir = tmp_dir("prop-determinism");
        let src = dir.join("src.jsonl");
        write_tape(&src, &tape);
        let map = |s: Step<'_, Ext>| -> Result<Emit<Ext>> {
            Ok(match &s.rec.event {
                Ext::Note(t) => Emit::Replace(Ext::Note(format!("{t}!"))),
                _ => Emit::Keep,
            })
        };
        let a = dir.join("a.jsonl");
        let b = dir.join("b.jsonl");
        rewrite::<Totals, Ext, Ext>(&src, &a, reduce_ext, Mode::Renumber, map).unwrap();
        rewrite::<Totals, Ext, Ext>(&src, &b, reduce_ext, Mode::Renumber, map).unwrap();
        prop_assert_eq!(read_bytes(&a), read_bytes(&b));
        let da = digest_pairs(&fold_dump::<Totals, Ext>(&a, reduce_ext).unwrap());
        let db = digest_pairs(&fold_dump::<Totals, Ext>(&b, reduce_ext).unwrap());
        prop_assert_eq!(da, db);
    }

    /// (e) Composition: rewrite(rewrite(x, f), g) equals one rewrite with g∘f,
    /// byte for byte.
    #[test]
    fn rewrites_compose(tape in tape_strategy(ext_event())) {
        fn f(e: &Ext) -> Ext {
            match e {
                Ext::Add { key, n } => Ext::Add { key: key.clone(), n: n.wrapping_add(1) },
                Ext::Note(s) => Ext::Note(format!("{s}~")),
                other => other.clone(),
            }
        }
        fn g(e: &Ext) -> Ext {
            match e {
                Ext::Add { key, n } => Ext::Add { key: key.clone(), n: n.wrapping_mul(2) },
                Ext::Pair { a, b } => Ext::Pair { a: a.wrapping_add(7), b: b.clone() },
                other => other.clone(),
            }
        }
        let dir = tmp_dir("prop-compose");
        let src = dir.join("src.jsonl");
        write_tape(&src, &tape);
        let mid = dir.join("mid.jsonl");
        rewrite::<Totals, Ext, Ext>(&src, &mid, reduce_ext, Mode::SameSeq, |s| {
            Ok(Emit::Replace(f(&s.rec.event)))
        })
        .unwrap();
        let twice = dir.join("twice.jsonl");
        rewrite::<Totals, Ext, Ext>(&mid, &twice, reduce_ext, Mode::SameSeq, |s| {
            Ok(Emit::Replace(g(&s.rec.event)))
        })
        .unwrap();
        let once = dir.join("once.jsonl");
        rewrite::<Totals, Ext, Ext>(&src, &once, reduce_ext, Mode::SameSeq, |s| {
            Ok(Emit::Replace(g(&f(&s.rec.event))))
        })
        .unwrap();
        prop_assert_eq!(read_bytes(&twice), read_bytes(&once));
    }
}

fn app_only(d: durable::Dump) -> durable::Dump {
    d.into_iter().filter(|(t, k, _)| app_keys_only(t, k)).collect()
}

/// (seq, from, to) of a sealed line, by parsing the envelope.
fn parse_env(line: &str) -> (u64, u64, u64) {
    let v: serde_json::Value = serde_json::from_str(line).unwrap();
    (
        v["seq"].as_u64().unwrap(),
        v["from"].as_u64().unwrap(),
        v["to"].as_u64().unwrap(),
    )
}
