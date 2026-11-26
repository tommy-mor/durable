//! The worked example from `examples/rewrite_round_paired.rs`, as a test: the
//! old fold and the rewritten tape agree, the arenas are state-dependent, and
//! a map that ignores the pre-record state is caught by the equivalence check.

#[allow(dead_code)]
#[path = "../examples/rewrite_round_paired.rs"]
mod worked;

mod common;

use common::{dir_names, read_bytes, tmp_dir};
use durable::testing::{app_keys_only, check_rewrite_equivalent};
use durable::{census, rewrite, Emit, Mode, Runtime};
use worked::*;

#[test]
fn round_began_becomes_round_paired_and_folds_identically() {
    let dir = tmp_dir("worked");
    let dst = run(&dir).expect("example runs");
    // The new tape holds the decisions: two RoundPaired records, with the
    // arenas the old rule produced from the state before each round.
    let lines = census::cat(&dst, 0).unwrap();
    let round1 = String::from_utf8(lines[5].clone()).unwrap();
    assert!(round1.contains(r#""type":"round_paired""#), "{round1}");
    assert!(
        round1.contains(r#""arenas":["ann vs bo","cy vs di"]"#),
        "{round1}"
    );
    assert!(
        round1.contains(r#""flushed":["ann","bo","cy","di"]"#),
        "{round1}"
    );
    let round2 = String::from_utf8(lines[7].clone()).unwrap();
    assert!(round2.contains(r#""arenas":["ed vs fay"]"#), "{round2}");
    // The Joined records were carried over byte for byte.
    let src_lines = census::cat(&dir.join("ladder.jsonl"), 0).unwrap();
    assert_eq!(lines[0], src_lines[0]);
    assert_eq!(lines[4], src_lines[4]);
}

#[test]
fn the_rewrite_report_counts_only_the_round_records() {
    let dir = tmp_dir("worked-report");
    let src = write_old_tape(&dir).unwrap();
    let before = read_bytes(&src);
    let dst = dir.join("out.jsonl");
    let report = rewrite::<Ladder, Old, New>(
        &src,
        &dst,
        reduce_old,
        Mode::SameSeq,
        map_old_to_new,
    )
    .unwrap();
    assert_eq!(report.replaced, 2);
    assert_eq!(read_bytes(&src), before);
    assert!(dst.exists(), "output written");
    assert!(dir_names(&dir).contains(&"out.jsonl".to_string()));
}

#[test]
fn a_map_that_ignores_the_pre_record_state_is_caught() {
    // The control: decide the arenas without looking at `before`. The fold
    // then differs, and the equivalence check says where.
    let dir = tmp_dir("worked-control");
    let src = write_old_tape(&dir).unwrap();
    let dst = dir.join("naive.jsonl");
    rewrite::<Ladder, Old, New>(
        &src,
        &dst,
        reduce_old,
        Mode::SameSeq,
        |s| {
            Ok(match &s.rec.event {
                Old::Joined { .. } => Emit::Keep,
                Old::RoundBegan { round, map } => Emit::Replace(New::RoundPaired {
                    round: *round,
                    map: map.clone(),
                    flushed: vec![],
                    arenas: vec![],
                }),
            })
        },
    )
    .unwrap();
    let diff = check_rewrite_equivalent::<Ladder, Old, New>(
        &src,
        &dst,
        reduce_old,
        reduce_new,
        app_keys_only,
    )
    .expect_err("ignoring the pre-record state must not be equivalent");
    assert!(diff.contains("first differing key"), "{diff}");
}

#[test]
fn the_new_writer_and_the_rewritten_tape_agree_on_projection_digest() {
    let dir = tmp_dir("worked-digest");
    let dst = run(&dir).unwrap();
    let rt = Runtime::open::<Ladder>(dir.join("proj-new"), &dst, reduce_new).unwrap();
    rt.verify().unwrap();
    let folded = durable::fold_dump::<Ladder, New>(&dst, reduce_new).unwrap();
    let app_rt: Vec<_> = rt
        .dump()
        .unwrap()
        .into_iter()
        .filter(|(t, k, _)| app_keys_only(t, k))
        .collect();
    let app_fold: Vec<_> = folded
        .into_iter()
        .filter(|(t, k, _)| app_keys_only(t, k))
        .collect();
    assert_eq!(app_rt, app_fold);
}
