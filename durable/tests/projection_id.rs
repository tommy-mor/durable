//! Projection identity: `open_with`, `OnMismatch`, the legacy `open` rules, the
//! tape lock, the crash matrix over the wipe/stamp/catch-up checkpoints, and a
//! random open/append/refuse state machine.

mod common;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use common::*;
use durable::testing::app_keys_only;
use durable::{
    fold_dump, shape_hash, Error, OnMismatch, OpenOptions, ProjectionId, ProjectionStamp, Result,
    Runtime, Shape, Tx,
};
use proptest::prelude::*;

type Dump = Vec<(Vec<u8>, Vec<u8>)>;

fn app(dump: &Dump) -> Dump {
    dump.iter()
        .filter(|(k, _)| app_keys_only(k))
        .cloned()
        .collect()
}

fn meta(dump: &Dump) -> Dump {
    dump.iter()
        .filter(|(k, _)| !app_keys_only(k))
        .cloned()
        .collect()
}

/// Paths of a fresh store: (projection dir, tape).
fn store_paths(dir: &Path) -> (PathBuf, PathBuf) {
    (dir.join("proj"), dir.join("tape.jsonl"))
}

fn alt_schema() -> Shape {
    Shape::record(vec![("other".to_string(), Shape::Leaf)])
}

fn open_ext(dir: &Path, id: u64, schema: Shape, on_mismatch: OnMismatch) -> Result<Runtime<Ext>> {
    let (db, log) = store_paths(dir);
    let mut o = OpenOptions::new(schema, ProjectionId::new(id), reduce_ext);
    o.on_mismatch = on_mismatch;
    Runtime::open_with(db, log, o)
}

fn seed_store(dir: &Path, id: u64, n: u64) {
    let rt = open_ext(dir, id, schema(), OnMismatch::Rebuild).unwrap();
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

fn counting_open(
    dir: &Path,
    id: u64,
    schema: Shape,
    mode: OnMismatch,
) -> Result<(Runtime<Ext>, Arc<AtomicUsize>)> {
    let calls = Arc::new(AtomicUsize::new(0));
    let c = calls.clone();
    let (db, log) = store_paths(dir);
    let mut o = OpenOptions::new(
        schema,
        ProjectionId::new(id),
        move |tx: &mut Tx, ev: &Ext| {
            c.fetch_add(1, Ordering::SeqCst);
            reduce_ext(tx, ev)
        },
    );
    o.on_mismatch = mode;
    Ok((Runtime::open_with(db, log, o)?, calls))
}

#[test]
#[cfg_attr(
    feature = "backend-memory",
    ignore = "ephemeral backend: projections do not persist across opens"
)]
fn fresh_store_is_stamped_and_reopen_with_same_id_does_not_replay() {
    let dir = tmp_dir("pid-stamp");
    seed_store(&dir, 1, 5);
    let (rt, calls) = counting_open(&dir, 1, schema(), OnMismatch::Rebuild).unwrap();
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "matching stamp must not replay"
    );
    assert_eq!(
        rt.projection_stamp(),
        Some(ProjectionStamp::new(1, &schema()))
    );
    assert_eq!(rt.applied().unwrap(), 5);
    let stored = meta(&rt.dump().unwrap());
    assert_eq!(stored.len(), 3, "applied + reducer + shape_hash");
}

#[test]
#[cfg_attr(
    feature = "backend-memory",
    ignore = "ephemeral backend: projections do not persist across opens"
)]
fn reducer_id_bump_rebuilds_from_zero() {
    let dir = tmp_dir("pid-bump");
    seed_store(&dir, 1, 5);
    let (rt, calls) = counting_open(&dir, 2, schema(), OnMismatch::Rebuild).unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 5, "every record re-folded");
    assert_eq!(rt.projection_stamp().unwrap().reducer, 2);
    assert_eq!(rt.applied().unwrap(), 5);
}

#[test]
#[cfg_attr(
    feature = "backend-memory",
    ignore = "ephemeral backend: projections do not persist across opens"
)]
fn shape_change_with_same_id_rebuilds() {
    let dir = tmp_dir("pid-shape");
    seed_store(&dir, 1, 4);
    let (rt, calls) = counting_open(&dir, 1, alt_schema(), OnMismatch::Rebuild).unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 4);
    assert_eq!(
        rt.projection_stamp().unwrap().shape_hash,
        shape_hash(&alt_schema())
    );
}

#[test]
#[cfg_attr(
    feature = "backend-memory",
    ignore = "ephemeral backend: projections do not persist across opens"
)]
fn refuse_mismatch_returns_typed_error_and_writes_nothing() {
    let dir = tmp_dir("pid-refuse");
    seed_store(&dir, 1, 4);
    let (db, log) = store_paths(&dir);
    let tape_before = read_bytes(&log);
    let stored_before = {
        let rt = open_ext(&dir, 1, schema(), OnMismatch::Refuse).unwrap();
        rt.dump().unwrap()
    };
    let err = match open_ext(&dir, 2, schema(), OnMismatch::Refuse) {
        Err(e) => e,
        Ok(_) => panic!("refuse must refuse"),
    };
    match err {
        Error::ProjectionMismatch { stored, wanted } => {
            assert_eq!(stored, Some(ProjectionStamp::new(1, &schema())));
            assert_eq!(wanted, ProjectionStamp::new(2, &schema()));
        }
        other => panic!("wrong error: {other:?}"),
    }
    assert_eq!(read_bytes(&log), tape_before, "tape untouched");
    let rt = open_ext(&dir, 1, schema(), OnMismatch::Refuse).unwrap();
    assert_eq!(rt.dump().unwrap(), stored_before, "projection untouched");
    drop((db, rt));
}

#[test]
#[cfg_attr(
    feature = "backend-memory",
    ignore = "ephemeral backend: projections do not persist across opens"
)]
fn wipe_happens_before_any_reducer_call() {
    // The old store has `sums["old"] = 1`. The new reducer refuses to run if
    // that key is visible: a rebuild must wipe it before the first call.
    let dir = tmp_dir("pid-wipe-first");
    let (db, log) = store_paths(&dir);
    {
        let old = Runtime::open_with(
            &db,
            &log,
            OpenOptions::new(schema(), ProjectionId::new(1), |tx: &mut Tx, _ev: &Ext| {
                tx.write(Totals::root().sums().key(&"old".to_string()).add(1));
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
        OpenOptions::new(
            schema(),
            ProjectionId::new(2),
            move |tx: &mut Tx, ev: &Ext| {
                if f.fetch_add(1, Ordering::SeqCst) == 0 {
                    let leftover = Totals::root().sums().key(&"old".to_string()).get(tx.db())?;
                    if leftover != 0 {
                        return Err(Error::Reducer(
                            "old layout visible to the new reducer".into(),
                        ));
                    }
                }
                reduce_ext(tx, ev)
            },
        ),
    );
    assert!(rt.is_ok(), "reducer saw the old projection: {:?}", rt.err());
    assert_eq!(first.load(Ordering::SeqCst), 1);
}

#[test]
#[cfg_attr(
    feature = "backend-memory",
    ignore = "ephemeral backend: projections do not persist across opens"
)]
fn legacy_store_is_rebuilt_once_and_refused_under_refuse() {
    let dir = tmp_dir("pid-legacy");
    let (db, log) = store_paths(&dir);
    {
        let legacy = Runtime::open(&db, &log, schema(), None, reduce_ext).unwrap();
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
    // Refuse: an unstamped store with applied > 0 has no identity to match.
    match open_ext(&dir, 1, schema(), OnMismatch::Refuse) {
        Err(Error::ProjectionMismatch { stored: None, .. }) => {}
        Err(other) => panic!("wrong error: {other:?}"),
        Ok(_) => panic!("refuse must refuse a legacy store"),
    }
    // Rebuild: replayed once, then stamped and quiet.
    let (rt, calls) = counting_open(&dir, 1, schema(), OnMismatch::Rebuild).unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    drop(rt);
    let (_rt, calls) = counting_open(&dir, 1, schema(), OnMismatch::Rebuild).unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[test]
#[cfg_attr(
    feature = "backend-memory",
    ignore = "ephemeral backend: projections do not persist across opens"
)]
fn legacy_open_accepts_reducer_zero_and_refuses_other_identities() {
    let dir = tmp_dir("pid-legacy-open");
    seed_store(&dir, 0, 2);
    let (db, log) = store_paths(&dir);
    Runtime::open(&db, &log, schema(), None, reduce_ext).expect("reducer 0 stamp is accepted");
    drop(open_ext(&dir, 0, schema(), OnMismatch::Rebuild).unwrap());

    let dir2 = tmp_dir("pid-legacy-open-2");
    seed_store(&dir2, 5, 2);
    let (db2, log2) = store_paths(&dir2);
    match Runtime::open(&db2, &log2, schema(), None, reduce_ext) {
        Err(Error::ProjectionMismatch {
            stored: Some(s),
            wanted,
        }) => {
            assert_eq!(s.reducer, 5);
            assert_eq!(wanted.reducer, 0);
        }
        Err(other) => panic!("wrong error: {other:?}"),
        Ok(_) => panic!("legacy open must refuse reducer 5"),
    }
}

#[test]
fn tape_is_locked_by_one_writer_at_a_time() {
    let dir = tmp_dir("pid-lock");
    seed_store(&dir, 1, 1);
    let first = open_ext(&dir, 1, schema(), OnMismatch::Rebuild).unwrap();
    match open_ext(&dir, 1, schema(), OnMismatch::Rebuild) {
        Err(Error::Log(msg)) => assert!(msg.contains("locked"), "{msg}"),
        Err(other) => panic!("wrong error: {other:?}"),
        Ok(_) => panic!("second writer must be refused"),
    }
    drop(first);
    open_ext(&dir, 1, schema(), OnMismatch::Rebuild).expect("lock released on drop");
}

#[test]
#[cfg_attr(
    feature = "backend-memory",
    ignore = "ephemeral backend: projections do not persist across opens"
)]
fn rebuild_restamps_identified_runtime() {
    let dir = tmp_dir("pid-rebuild-restamp");
    seed_store(&dir, 3, 3);
    let (rt, _) = counting_open(&dir, 3, schema(), OnMismatch::Rebuild).unwrap();
    let before = rt.dump().unwrap();
    rt.rebuild().unwrap();
    assert_eq!(
        rt.projection_stamp(),
        Some(ProjectionStamp::new(3, &schema()))
    );
    assert_eq!(rt.dump().unwrap(), before, "rebuild is deterministic");
    rt.verify().unwrap();
}

#[test]
fn application_state_equals_the_fold_of_the_tape() {
    let dir = tmp_dir("pid-fold-eq");
    seed_store(&dir, 4, 7);
    let (rt, _) = counting_open(&dir, 4, schema(), OnMismatch::Rebuild).unwrap();
    let (_, log) = store_paths(&dir);
    let folded = fold_dump::<Ext>(&log, schema(), reduce_ext).unwrap();
    assert_eq!(app(&rt.dump().unwrap()), app(&folded));
    assert!(!app(&rt.dump().unwrap()).is_empty());
}

#[test]
fn stored_identity_matches_the_wanted_one_wherever_applied_is_nonzero() {
    // Invariant the crash analysis relies on: a store with applied > 0 is
    // stamped with exactly the identity that applied it.
    let dir = tmp_dir("pid-invariant");
    seed_store(&dir, 2, 2);
    let (rt, _) = counting_open(&dir, 2, schema(), OnMismatch::Rebuild).unwrap();
    let m = meta(&rt.dump().unwrap());
    let decoded: Vec<u64> = m
        .iter()
        .map(|(_, v)| ciborium::de::from_reader::<u64, _>(v.as_slice()).unwrap())
        .collect();
    // applied (2 records), reducer id 2, and the shape hash: exactly these.
    let mut sorted = decoded.clone();
    sorted.sort_unstable();
    let mut want = vec![2, 2, shape_hash(&schema())];
    want.sort_unstable();
    assert_eq!(sorted, want);
}

#[test]
fn shape_hash_is_pinned_across_releases() {
    // Golden values. Changing the canonical encoding changes these and forces
    // a rebuild of every projection: do not "fix" these numbers casually.
    let leaf = shape_hash(&Shape::Leaf);
    let rec = shape_hash(&Shape::record(vec![("a".into(), Shape::Leaf)]));
    let rec_b = shape_hash(&Shape::record(vec![("b".into(), Shape::Leaf)]));
    assert_ne!(leaf, rec);
    assert_ne!(rec, rec_b, "field names are part of the hash");
    // Pinned: the canonical encoding is part of the on-disk identity.
    assert_eq!(leaf, 0x67cc_c757_d09a_078d);
    assert_eq!(rec, 0xe2a8_c014_3eef_0783);
    assert_eq!(shape_hash(&schema()), 0x3cce_31ac_3b59_e7c5);
}

#[test]
fn projection_id_and_mismatch_display_are_stable() {
    assert_eq!(ProjectionId::default(), ProjectionId::new(0));
    assert_eq!(OnMismatch::default(), OnMismatch::Rebuild);
    let s = ProjectionStamp {
        reducer: 7,
        shape_hash: 0xabc,
    };
    assert_eq!(s.to_string(), "reducer=7 shape=0000000000000abc");
    let e = Error::ProjectionMismatch {
        stored: None,
        wanted: s,
    };
    assert!(e.to_string().contains("wanted reducer=7"), "{e}");
}

// ---- crash matrix over open_with's checkpoints ----

#[cfg(feature = "fault-injection")]
#[test]
#[cfg_attr(
    feature = "backend-memory",
    ignore = "ephemeral backend: projections do not persist across opens"
)]
fn crash_matrix_over_rebuild_and_catch_up() {
    use durable::fault;

    let reference = {
        // Reference: a clean rebuild to identity 2 in a sibling store.
        let refdir = tmp_dir("pid-crash-ref");
        seed_store(&refdir, 1, 4);
        fault::arm(u64::MAX);
        let (rt, _) = counting_open(&refdir, 2, schema(), OnMismatch::Rebuild).unwrap();
        let labels = fault::labels();
        fault::disarm();
        (app(&rt.dump().unwrap()), labels)
    };
    let (reference_app, labels) = reference;
    assert!(labels.contains(&"projection.wipe"));
    assert!(labels.contains(&"projection.stamp"));
    assert!(labels.iter().filter(|l| **l == "projection.apply").count() == 4);

    for n in 0..labels.len() as u64 {
        // Each run starts from the same store: stamped 1 with 4 records.
        let run_dir = tmp_dir("pid-crash-run");
        seed_store(&run_dir, 1, 4);
        let tape_before = read_bytes(&run_dir.join("tape.jsonl"));
        fault::arm(n);
        let outcome = counting_open(&run_dir, 2, schema(), OnMismatch::Rebuild);
        fault::disarm();
        let label = labels[n as usize];
        assert!(
            outcome.is_err(),
            "fault at {n} ({label}) must fail the open"
        );

        // Whatever the crash left, the next clean open must converge, with the
        // identity invariant holding on every intermediate state we can see.
        let (rt, calls) = counting_open(&run_dir, 2, schema(), OnMismatch::Rebuild)
            .unwrap_or_else(|e| panic!("reopen after {label} failed: {e}"));
        assert_eq!(
            rt.projection_stamp(),
            Some(ProjectionStamp::new(2, &schema()))
        );
        assert_eq!(
            app(&rt.dump().unwrap()),
            reference_app,
            "after crash at {label}"
        );
        let m = meta(&rt.dump().unwrap());
        assert_eq!(
            m.len(),
            3,
            "after crash at {label}: applied, reducer, shape"
        );
        let calls = calls.load(Ordering::SeqCst);
        assert!(calls <= 4, "after crash at {label}: {calls} replays");
        assert_eq!(
            read_bytes(&run_dir.join("tape.jsonl")),
            tape_before,
            "the tape itself is never touched by a crashed or recovered open"
        );
    }
}

#[cfg(feature = "fault-injection")]
#[test]
#[cfg_attr(
    feature = "backend-memory",
    ignore = "ephemeral backend: projections do not persist across opens"
)]
fn crash_matrix_on_a_fresh_store() {
    use durable::fault;
    fault::arm(u64::MAX);
    drop(
        open_ext(
            &tmp_dir("pid-crash-fresh-ref"),
            1,
            schema(),
            OnMismatch::Rebuild,
        )
        .unwrap(),
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
        let r = open_ext(&run, 1, schema(), OnMismatch::Rebuild);
        fault::disarm();
        assert!(r.is_err(), "fresh-store fault at {n} must fail");
        let rt = open_ext(&run, 1, schema(), OnMismatch::Refuse)
            .unwrap_or_else(|e| panic!("fresh reopen after {n} failed: {e}"));
        assert_eq!(
            rt.projection_stamp(),
            Some(ProjectionStamp::new(1, &schema()))
        );
        assert_eq!(rt.applied().unwrap(), 0);
    }
}

// ---- random state machine ----

#[derive(Debug, Clone)]
enum Step {
    Open {
        id: u64,
        alt_shape: bool,
        refuse: bool,
    },
    Append(u8),
}

fn step_strategy() -> impl Strategy<Value = Step> {
    prop_oneof![
        (0u64..3, any::<bool>(), any::<bool>()).prop_map(|(id, alt_shape, refuse)| Step::Open {
            id,
            alt_shape,
            refuse,
        }),
        (1u8..4).prop_map(Step::Append),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Random opens (changing ids, shapes, modes) and appends: after every
    /// step the store is either untouched by a refusal or stamped with the
    /// identity that applied it, and application state equals the tape's fold.
    #[test]
    fn random_open_append_sequences_keep_identity_and_state_consistent(
        steps in proptest::collection::vec(step_strategy(), 1..12)
    ) {
        let dir = tmp_dir("pid-prop");
        let (_, log) = store_paths(&dir);
        let mut current: Option<(u64, bool)> = None; // last successful identity
        let mut n_records = 0u64;
        for step in steps {
            match step {
                Step::Open { id, alt_shape, refuse } => {
                    let shape = if alt_shape { alt_schema() } else { schema() };
                    let mode = if refuse { OnMismatch::Refuse } else { OnMismatch::Rebuild };
                    let before = current;
                    match open_ext(&dir, id, shape.clone(), mode) {
                        Ok(rt) => {
                            current = Some((id, alt_shape));
                            prop_assert_eq!(rt.projection_stamp(), Some(ProjectionStamp::new(id, &shape)));
                            prop_assert_eq!(rt.applied().unwrap(), n_records);
                            let dump = rt.dump().unwrap();
                            let m = meta(&dump);
                            if n_records > 0 {
                                prop_assert_eq!(m.len(), 3, "applied>0 needs identity");
                            }
                            let folded = fold_dump::<Ext>(&log, schema(), reduce_ext).unwrap();
                            prop_assert_eq!(app(&dump), app(&folded));
                        }
                        Err(Error::ProjectionMismatch { stored, wanted }) => {
                            // Refusal: the store keeps the identity it had.
                            prop_assert_eq!(mode, OnMismatch::Refuse);
                            prop_assert_eq!(wanted, ProjectionStamp::new(id, &shape));
                            prop_assert!(stored.is_some() || n_records == 0);
                            prop_assert_eq!(before.is_some(), stored.is_some());
                            let (bid, balt) = before.unwrap();
                            let still = open_ext(
                                &dir,
                                bid,
                                if balt { alt_schema() } else { schema() },
                                OnMismatch::Refuse,
                            );
                            prop_assert!(still.is_ok(), "refusal must not disturb the store");
                        }
                        Err(other) => prop_assert!(false, "unexpected error {other:?}"),
                    }
                }
                Step::Append(k) => {
                    if let Some((id, alt)) = current {
                        let rt = open_ext(&dir, id, if alt { alt_schema() } else { schema() }, OnMismatch::Refuse);
                        if let Ok(rt) = rt {
                            for i in 0..k {
                                rt.append(Ext::Add { key: "p".into(), n: i as u64 + 1 }).unwrap();
                                n_records += 1;
                            }
                        }
                    } else {
                        // No identity yet: write through a fresh default-id store.
                        let rt = open_ext(&dir, 0, schema(), OnMismatch::Rebuild).unwrap();
                        for i in 0..k {
                            rt.append(Ext::Add { key: "p".into(), n: i as u64 + 1 }).unwrap();
                            n_records += 1;
                        }
                        current = Some((0, false));
                    }
                }
            }
        }
    }
}

/// The memory backend is ephemeral: every open starts from an empty projection,
/// so the tape is replayed on each open and the identity is stamped afresh.
/// This is the guarantee it does give, and it is what callers can rely on.
#[cfg(feature = "backend-memory")]
#[test]
fn ephemeral_backend_replays_the_tape_on_every_open() {
    let dir = tmp_dir("pid-ephemeral");
    seed_store(&dir, 1, 3);
    let (rt, calls) = counting_open(&dir, 1, schema(), OnMismatch::Refuse).unwrap();
    assert_eq!(
        calls.load(Ordering::SeqCst),
        3,
        "nothing persisted, so everything replays"
    );
    assert_eq!(
        rt.projection_stamp(),
        Some(ProjectionStamp::new(1, &schema()))
    );
    assert_eq!(rt.applied().unwrap(), 3);
}
