//! Projection identity: `open_with`, `OnMismatch`, `open_unmanaged`, the
//! tape lock, the crash matrix over wipe/stamp/catch-up checkpoints, and a
//! random open/append/refuse state machine.

mod common;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use common::*;
use durable::testing::app_keys_only;
use durable::{
    fold_dump, schema_hash, Dump, Error, OnMismatch, OpenOptions, ProjectionId, ProjectionStamp,
    Result, Runtime, Schema, Tx,
};
use proptest::prelude::*;

fn app(dump: &Dump) -> Dump {
    dump.iter()
        .filter(|(t, k, _)| app_keys_only(t, k))
        .cloned()
        .collect()
}

fn meta(dump: &Dump) -> Dump {
    dump.iter()
        .filter(|(t, k, _)| !app_keys_only(t, k))
        .cloned()
        .collect()
}

fn store_paths(dir: &Path) -> (PathBuf, PathBuf) {
    (dir.join("proj"), dir.join("tape.jsonl"))
}

fn open_ext<S: Schema>(dir: &Path, id: u64, on_mismatch: OnMismatch) -> Result<Runtime<Ext>> {
    let (db, log) = store_paths(dir);
    let o = OpenOptions::new::<S>(ProjectionId::new(id), reduce_ext).on_mismatch(on_mismatch);
    Runtime::open_with(db, log, o)
}

fn seed_store(dir: &Path, id: u64, n: u64) {
    let rt = open_ext::<Totals>(dir, id, OnMismatch::Rebuild).unwrap();
    for i in 0..n {
        let ev = match i % 3 {
            0 => Ext::Add {
                key: format!("k{}", i % 2),
                n: i + 1,
            },
            1 => Ext::Note(format!("note {i}")),
            _ => Ext::Reset,
        };
        rt.append(ev).unwrap();
    }
}

fn counting_open<S: Schema>(
    dir: &Path,
    id: u64,
    mode: OnMismatch,
) -> Result<(Runtime<Ext>, Arc<AtomicUsize>)> {
    let calls = Arc::new(AtomicUsize::new(0));
    let c = calls.clone();
    let (db, log) = store_paths(dir);
    let o = OpenOptions::new::<S>(ProjectionId::new(id), move |tx: &mut Tx, ev: &Ext| {
        c.fetch_add(1, Ordering::SeqCst);
        reduce_ext(tx, ev)
    })
    .on_mismatch(mode);
    Ok((Runtime::open_with(db, log, o)?, calls))
}

#[test]
fn fresh_store_is_stamped_and_reopen_with_same_id_does_not_replay() {
    let dir = tmp_dir("pid-stamp");
    seed_store(&dir, 1, 5);
    let (rt, calls) = counting_open::<Totals>(&dir, 1, OnMismatch::Rebuild).unwrap();
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "matching stamp must not replay"
    );
    assert_eq!(
        rt.projection_stamp(),
        Some(ProjectionStamp::for_schema::<Totals>(1))
    );
    assert_eq!(rt.applied().unwrap(), 5);
    let stored = meta(&rt.dump().unwrap());
    // applied + applied_byte_offset + stamp_reducer + stamp_schema
    assert_eq!(stored.len(), 4);
}

#[test]
fn reducer_id_bump_rebuilds_from_zero() {
    let dir = tmp_dir("pid-bump");
    seed_store(&dir, 1, 5);
    let (rt, calls) = counting_open::<Totals>(&dir, 2, OnMismatch::Rebuild).unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 5, "every record re-folded");
    assert_eq!(rt.projection_stamp().unwrap().reducer, 2);
    assert_eq!(rt.applied().unwrap(), 5);
}

#[test]
fn schema_change_with_same_id_rebuilds() {
    let dir = tmp_dir("pid-shape");
    seed_store(&dir, 1, 4);
    let (rt, calls) = counting_open::<AltStore>(&dir, 1, OnMismatch::Rebuild).unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 4);
    assert_eq!(
        rt.projection_stamp().unwrap().schema_hash,
        schema_hash::<AltStore>()
    );
}

#[test]
fn refuse_mismatch_returns_typed_error_and_writes_nothing() {
    let dir = tmp_dir("pid-refuse");
    seed_store(&dir, 1, 4);
    let (db, log) = store_paths(&dir);
    let tape_before = read_bytes(&log);
    let stored_before = {
        let rt = open_ext::<Totals>(&dir, 1, OnMismatch::Refuse).unwrap();
        rt.dump().unwrap()
    };
    let err = match open_ext::<Totals>(&dir, 2, OnMismatch::Refuse) {
        Err(e) => e,
        Ok(_) => panic!("refuse must refuse"),
    };
    match err {
        Error::ProjectionMismatch { stored, wanted } => {
            assert_eq!(stored, Some(ProjectionStamp::for_schema::<Totals>(1)));
            assert_eq!(wanted, ProjectionStamp::for_schema::<Totals>(2));
        }
        other => panic!("wrong error: {other:?}"),
    }
    assert_eq!(read_bytes(&log), tape_before, "tape untouched");
    let rt = open_ext::<Totals>(&dir, 1, OnMismatch::Refuse).unwrap();
    assert_eq!(rt.dump().unwrap(), stored_before, "projection untouched");
    drop((db, rt));
}

#[test]
fn wipe_happens_before_any_reducer_call() {
    let dir = tmp_dir("pid-wipe-first");
    let (db, log) = store_paths(&dir);
    {
        let old = Runtime::open_with(
            &db,
            &log,
            OpenOptions::new::<Totals>(ProjectionId::new(1), |tx: &mut Tx, _ev: &Ext| {
                Totals::root().sums().add(tx, "old", 1)?;
                Ok(())
            }),
        )
        .unwrap();
        old.append(Ext::Note("x".into())).unwrap();
    }
    let first = Arc::new(AtomicUsize::new(0));
    let f = first.clone();
    let rt = Runtime::open_with(
        &db,
        &log,
        OpenOptions::new::<Totals>(ProjectionId::new(2), move |tx: &mut Tx, ev: &Ext| {
            if f.fetch_add(1, Ordering::SeqCst) == 0 {
                let leftover = Totals::root().sums().get(tx, "old")?;
                if leftover != 0 {
                    return Err(Error::Reducer(
                        "old layout visible to the new reducer".into(),
                    ));
                }
            }
            reduce_ext(tx, ev)
        }),
    );
    assert!(rt.is_ok(), "reducer saw the old projection: {:?}", rt.err());
    assert_eq!(first.load(Ordering::SeqCst), 1);
}

#[test]
fn unmanaged_store_is_rebuilt_once_and_refused_under_refuse() {
    let dir = tmp_dir("pid-legacy");
    let (db, log) = store_paths(&dir);
    {
        let legacy = Runtime::open_unmanaged(&db, &log, reduce_ext).unwrap();
        for i in 0..3 {
            legacy
                .append(Ext::Add {
                    key: "a".into(),
                    n: i,
                })
                .unwrap();
        }
        assert!(legacy.projection_stamp().is_none());
    }
    match open_ext::<Totals>(&dir, 1, OnMismatch::Refuse) {
        Err(Error::ProjectionMismatch { stored: None, .. }) => {}
        Err(other) => panic!("wrong error: {other:?}"),
        Ok(_) => panic!("refuse must refuse an unstamped store"),
    }
    let (rt, calls) = counting_open::<Totals>(&dir, 1, OnMismatch::Rebuild).unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    drop(rt);
    let (_rt, calls) = counting_open::<Totals>(&dir, 1, OnMismatch::Rebuild).unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[test]
fn unmanaged_open_accepts_reducer_zero_and_refuses_other_identities() {
    let dir = tmp_dir("pid-legacy-open");
    seed_store(&dir, 0, 2);
    let (db, log) = store_paths(&dir);
    Runtime::open_unmanaged(&db, &log, reduce_ext).expect("reducer 0 stamp is accepted");
    drop(open_ext::<Totals>(&dir, 0, OnMismatch::Rebuild).unwrap());

    let dir2 = tmp_dir("pid-legacy-open-2");
    seed_store(&dir2, 5, 2);
    let (db2, log2) = store_paths(&dir2);
    match Runtime::open_unmanaged(&db2, &log2, reduce_ext) {
        Err(Error::ProjectionMismatch {
            stored: Some(s),
            wanted,
        }) => {
            assert_eq!(s.reducer, 5);
            assert_eq!(wanted.reducer, 0);
        }
        Err(other) => panic!("wrong error: {other:?}"),
        Ok(_) => panic!("unmanaged open must refuse reducer 5"),
    }
}

#[test]
fn tape_is_locked_by_one_writer_at_a_time() {
    let dir = tmp_dir("pid-lock");
    seed_store(&dir, 1, 1);
    let first = open_ext::<Totals>(&dir, 1, OnMismatch::Rebuild).unwrap();
    match open_ext::<Totals>(&dir, 1, OnMismatch::Rebuild) {
        Err(Error::Log(msg)) => assert!(msg.contains("locked"), "{msg}"),
        Err(other) => panic!("wrong error: {other:?}"),
        Ok(_) => panic!("second writer must be refused"),
    }
    drop(first);
    open_ext::<Totals>(&dir, 1, OnMismatch::Rebuild).expect("lock released on drop");
}

#[test]
fn rebuild_restamps_identified_runtime() {
    let dir = tmp_dir("pid-rebuild-restamp");
    seed_store(&dir, 3, 3);
    let (rt, _) = counting_open::<Totals>(&dir, 3, OnMismatch::Rebuild).unwrap();
    let before = rt.dump().unwrap();
    rt.rebuild().unwrap();
    assert_eq!(
        rt.projection_stamp(),
        Some(ProjectionStamp::for_schema::<Totals>(3))
    );
    assert_eq!(rt.dump().unwrap(), before, "rebuild is deterministic");
    rt.verify().unwrap();
}

#[test]
fn application_state_equals_the_fold_of_the_tape() {
    let dir = tmp_dir("pid-fold-eq");
    seed_store(&dir, 4, 7);
    let (rt, _) = counting_open::<Totals>(&dir, 4, OnMismatch::Rebuild).unwrap();
    let (_, log) = store_paths(&dir);
    let folded = fold_dump::<Totals, Ext>(&log, reduce_ext).unwrap();
    assert_eq!(app(&rt.dump().unwrap()), app(&folded));
    assert!(!app(&rt.dump().unwrap()).is_empty());
}

#[test]
fn projection_id_and_mismatch_display_are_stable() {
    assert_eq!(ProjectionId::default(), ProjectionId::new(0));
    assert_eq!(OnMismatch::default(), OnMismatch::Rebuild);
    let s = ProjectionStamp {
        reducer: 7,
        schema_hash: 0xabc,
    };
    assert_eq!(s.to_string(), "reducer=7 schema=0000000000000abc");
    let e = Error::ProjectionMismatch {
        stored: None,
        wanted: s,
    };
    assert!(e.to_string().contains("wanted reducer=7"), "{e}");
}

// ---- crash matrix over open_with's checkpoints ----

#[cfg(feature = "fault-injection")]
#[test]
fn crash_matrix_over_rebuild_and_catch_up() {
    use durable::fault;

    let reference = {
        let refdir = tmp_dir("pid-crash-ref");
        seed_store(&refdir, 1, 4);
        fault::arm(u64::MAX);
        let (rt, _) = counting_open::<Totals>(&refdir, 2, OnMismatch::Rebuild).unwrap();
        let labels = fault::labels();
        fault::disarm();
        (app(&rt.dump().unwrap()), labels)
    };
    let (reference_app, labels) = reference;
    assert!(labels.contains(&"projection.wipe"));
    assert!(labels.contains(&"projection.stamp"));
    assert_eq!(
        labels.iter().filter(|l| **l == "projection.apply").count(),
        4
    );

    for n in 0..labels.len() as u64 {
        let run_dir = tmp_dir("pid-crash-run");
        seed_store(&run_dir, 1, 4);
        let tape_before = read_bytes(&run_dir.join("tape.jsonl"));
        fault::arm(n);
        let outcome = counting_open::<Totals>(&run_dir, 2, OnMismatch::Rebuild);
        fault::disarm();
        let label = labels[n as usize];
        assert!(
            outcome.is_err(),
            "fault at {n} ({label}) must fail the open"
        );

        let (rt, calls) = counting_open::<Totals>(&run_dir, 2, OnMismatch::Rebuild)
            .unwrap_or_else(|e| panic!("reopen after {label} failed: {e}"));
        assert_eq!(
            rt.projection_stamp(),
            Some(ProjectionStamp::for_schema::<Totals>(2))
        );
        assert_eq!(
            app(&rt.dump().unwrap()),
            reference_app,
            "after crash at {label}"
        );
        let calls = calls.load(Ordering::SeqCst);
        assert!(calls <= 4, "after crash at {label}: {calls} replays");
        assert_eq!(
            read_bytes(&run_dir.join("tape.jsonl")),
            tape_before,
            "tape never touched by crashed/recovered open"
        );
    }
}

#[cfg(feature = "fault-injection")]
#[test]
fn crash_matrix_on_a_fresh_store() {
    use durable::fault;
    fault::arm(u64::MAX);
    drop(
        open_ext::<Totals>(&tmp_dir("pid-crash-fresh-ref"), 1, OnMismatch::Rebuild).unwrap(),
    );
    let labels = fault::labels();
    fault::disarm();
    assert_eq!(
        labels,
        vec!["projection.wipe", "projection.stamp", "projection.stamped"]
    );
    for n in 0..labels.len() as u64 {
        let run = tmp_dir("pid-crash-fresh-run");
        fault::arm(n);
        let r = open_ext::<Totals>(&run, 1, OnMismatch::Rebuild);
        fault::disarm();
        assert!(r.is_err(), "fresh-store fault at {n} must fail");
        let rt = open_ext::<Totals>(&run, 1, OnMismatch::Refuse)
            .unwrap_or_else(|e| panic!("fresh reopen after {n} failed: {e}"));
        assert_eq!(
            rt.projection_stamp(),
            Some(ProjectionStamp::for_schema::<Totals>(1))
        );
        assert_eq!(rt.applied().unwrap(), 0);
    }
}

// ---- random state machine ----

#[derive(Debug, Clone)]
enum Step {
    Open {
        id: u64,
        alt_schema: bool,
        refuse: bool,
    },
    Append(u8),
}

fn step_strategy() -> impl Strategy<Value = Step> {
    prop_oneof![
        (0u64..3, any::<bool>(), any::<bool>()).prop_map(|(id, alt_schema, refuse)| Step::Open {
            id,
            alt_schema,
            refuse,
        }),
        (1u8..4).prop_map(Step::Append),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    #[test]
    fn random_open_append_sequences_keep_identity_and_state_consistent(
        steps in proptest::collection::vec(step_strategy(), 1..12)
    ) {
        let dir = tmp_dir("pid-prop");
        let (_, log) = store_paths(&dir);
        let mut current: Option<(u64, bool)> = None;
        let mut n_records = 0u64;
        for step in steps {
            match step {
                Step::Open { id, alt_schema, refuse } => {
                    let mode = if refuse { OnMismatch::Refuse } else { OnMismatch::Rebuild };
                    let res = if alt_schema {
                        open_ext::<AltStore>(&dir, id, mode)
                    } else {
                        open_ext::<Totals>(&dir, id, mode)
                    };
                    let wanted_stamp = if alt_schema {
                        ProjectionStamp::for_schema::<AltStore>(id)
                    } else {
                        ProjectionStamp::for_schema::<Totals>(id)
                    };
                    match res {
                        Ok(rt) => {
                            current = Some((id, alt_schema));
                            prop_assert_eq!(rt.projection_stamp(), Some(wanted_stamp));
                            prop_assert_eq!(rt.applied().unwrap(), n_records);
                            let dump = rt.dump().unwrap();
                            let folded = fold_dump::<Totals, Ext>(&log, reduce_ext).unwrap();
                            prop_assert_eq!(app(&dump), app(&folded));
                        }
                        Err(Error::ProjectionMismatch { stored, wanted }) => {
                            prop_assert_eq!(mode, OnMismatch::Refuse);
                            prop_assert_eq!(wanted, wanted_stamp);
                            prop_assert!(stored.is_some() || n_records == 0);
                            prop_assert!(current.is_some());
                        }
                        Err(other) => return Err(TestCaseError::fail(format!("unexpected err: {other}"))),
                    }
                }
                Step::Append(n) => {
                    let (id, alt) = current.unwrap_or((0, false));
                    let rt = if alt {
                        open_ext::<AltStore>(&dir, id, OnMismatch::Rebuild).unwrap()
                    } else {
                        open_ext::<Totals>(&dir, id, OnMismatch::Rebuild).unwrap()
                    };
                    current = Some((id, alt));
                    for i in 0..n as u64 {
                        rt.append(Ext::Add { key: format!("k{i}"), n: i + 1 }).unwrap();
                        n_records += 1;
                    }
                    rt.verify().unwrap();
                }
            }
        }
    }
}
