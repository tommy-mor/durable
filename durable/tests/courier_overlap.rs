//! Courier invalidation soundness: does prefix overlap catch every
//! read-changing write?
//!
//! The courier's load-bearing bridge is one claim: a commit's write set
//! (exact keys and range prefixes, handed over as the RocksDB batch) and
//! a subscribed read's range (the query's resolved byte prefix) overlap
//! **whenever the write could change the read's result**. Missed overlap
//! is a lost repaint; extra overlap is just a redundant one.
//!
//! These tests attack the claim two ways: targeted edge cases at the
//! codec/layout level (sibling disjointness, tag confusion, deletes,
//! metadata), and a seeded fuzz over schemas × writes × reads asserting
//! `changed ⟹ overlap` on every iteration.

use ciborium::value::Integer;
use ciborium::Value;
use proptest::prelude::*;
use tempfile::TempDir;

use durable::{
    dynpath::{add, clear, delete, push, put},
    navs_for, one, query::has, select, subtree, Db, Durability, Nav, Op, Query, Shape,
};

// ---------------------------------------------------------------------------
// The bridge, in miniature
// ---------------------------------------------------------------------------

fn overlap(a: &[u8], b: &[u8]) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

/// Write set of one commit: exact keys for point ops, prefixes for range
/// ops and deferred appends (resolved under the same prefix at commit).
fn write_set(writes: &[durable::Write]) -> Vec<Vec<u8>> {
    writes
        .iter()
        .map(|w| match w.op() {
            Op::Put { key, .. } | Op::Delete { key } | Op::Merge { key, .. } => key.clone(),
            Op::ListPush { prefix, .. }
            | Op::DequePushBack { prefix, .. }
            | Op::DequePushFront { prefix, .. }
            | Op::DeletePrefix { prefix } => prefix.clone(),
        })
        .collect()
}

/// Read range of a query: the resolved prefix of its longest point nav
/// run. Collecting navs fan out under that prefix (scans, per-key foci,
/// predicate evaluation from foci, collection metas) — everything a read
/// touches is at or under it. The empty prefix (root) matches all.
fn read_prefix(schema: &Shape, navs: &[Nav]) -> Vec<u8> {
    let point: Vec<Nav> = navs
        .iter()
        .take_while(|n| matches!(n, Nav::Field(_) | Nav::Key(_) | Nav::Index(_)))
        .cloned()
        .collect();
    match durable::dynpath::resolve(schema, None, &point) {
        Ok(loc) => loc.prefix,
        // Writes reject Leaf-interior descent, but reads go there
        // in-memory off the leaf value: the leaf's own key covers it.
        Err(_) => {
            let mut backoff = point.clone();
            while !backoff.is_empty() {
                backoff.pop();
                if let Ok(loc) = durable::dynpath::resolve(schema, None, &backoff) {
                    return loc.prefix;
                }
            }
            Vec::new()
        }
    }
}

fn invalidated(schema: &Shape, navs: &[Nav], writes: &[durable::Write]) -> bool {
    let rp = read_prefix(schema, navs);
    write_set(writes).iter().any(|w| overlap(w, &rp))
}

// ---------------------------------------------------------------------------
// Targeted edge cases
// ---------------------------------------------------------------------------

fn open() -> (TempDir, Db) {
    let dir = TempDir::new().unwrap();
    let db = Db::open(dir.path()).unwrap();
    (dir, db)
}

fn t(s: &str) -> Value {
    Value::Text(s.into())
}
fn n(i: i64) -> Value {
    Value::Integer(Integer::from(i))
}

fn shapes() -> Vec<(&'static str, Shape)> {
    vec![
        (
            "users",
            Shape::record(vec![
                (
                    "users".into(),
                    Shape::map(Shape::record(vec![
                        ("name".into(), Shape::Leaf),
                        ("age".into(), Shape::Leaf),
                    ])),
                ),
                ("total".into(), Shape::Sum),
                ("title".into(), Shape::Leaf),
            ]),
        ),
        (
            "feed",
            Shape::record(vec![
                ("feed".into(), Shape::list(Shape::Leaf)),
                ("scores".into(), Shape::map(Shape::Sum)),
                (
                    "meta".into(),
                    Shape::record(vec![
                        ("cursor".into(), Shape::Leaf),
                        ("n".into(), Shape::Sum),
                    ]),
                ),
            ]),
        ),
        (
            "mixed",
            Shape::record(vec![
                ("dq".into(), Shape::deque(Shape::Leaf)),
                ("m".into(), Shape::map(Shape::list(Shape::Leaf))),
                (
                    "r".into(),
                    Shape::record(vec![
                        ("a".into(), Shape::Leaf),
                        ("b".into(), Shape::Leaf),
                    ]),
                ),
            ]),
        ),
        (
            "deep",
            Shape::record(vec![(
                "l".into(),
                Shape::list(Shape::map(Shape::record(vec![("x".into(), Shape::Leaf)]))),
            )]),
        ),
    ]
}

#[test]
fn sibling_subtrees_never_overlap() {
    // 17000 fields so field ids cross uvarint widths (0, 1, 127,
    // 128, 16383, 16384): a shorter id encoding must never prefix a
    // longer one.
    let fields: Vec<(String, Shape)> =
        (0..17000).map(|i| (format!("f{i}"), Shape::Leaf)).collect();
    let schema = Shape::record(vec![
        ("wide".into(), Shape::record(fields)),
        ("m".into(), Shape::map(Shape::Leaf)),
        ("l".into(), Shape::list(Shape::Leaf)),
    ]);
    let root = durable::dynpath::resolve(&schema, None, &[]).unwrap().prefix;
    assert!(root.is_empty());

    let wide = durable::dynpath::resolve(&schema, None, &[Nav::Field("wide".into())])
        .unwrap()
        .prefix;
    let mut kids = Vec::new();
    for i in [0u64, 1, 5, 126, 127, 128, 129, 1000, 16383, 16384, 16999] {
        let navs = vec![Nav::Field("wide".into()), Nav::Field(format!("f{i}"))];
        let p = durable::dynpath::resolve(&schema, None, &navs).unwrap().prefix;
        assert!(overlap(&p, &wide), "child must overlap its parent");
        kids.push(p);
    }
    for (i, a) in kids.iter().enumerate() {
        for b in &kids[i + 1..] {
            assert!(!overlap(a, b), "sibling fields overlap: {a:?} vs {b:?}");
        }
    }
    // record vs map vs list subtrees under one parent are disjoint too.
    let m = durable::dynpath::resolve(&schema, None, &[Nav::Field("m".into())])
        .unwrap()
        .prefix;
    let l = durable::dynpath::resolve(&schema, None, &[Nav::Field("l".into())])
        .unwrap()
        .prefix;
    assert!(!overlap(&wide, &m));
    assert!(!overlap(&wide, &l));
    assert!(!overlap(&m, &l));
}

#[test]
fn adversarial_segments_dont_confuse_siblings() {
    // field id 0 encodes as [0x00]; a list index opens with TAG_INDEX
    // (0x00); map key int 0 encodes as CBOR [0x00]. As bare segments they
    // share prefixes — as length-prefixed child keys they must not.
    let schema = Shape::record(vec![
        ("rec".into(), Shape::record(vec![("f0".into(), Shape::Leaf)])),
        ("map".into(), Shape::map(Shape::Leaf)),
        ("list".into(), Shape::list(Shape::Leaf)),
    ]);
    let f0 = durable::dynpath::resolve(
        &schema,
        None,
        &[Nav::Field("rec".into()), Nav::Field("f0".into())],
    )
    .unwrap()
    .prefix;
    let k0 = durable::dynpath::resolve(&schema, None, &[Nav::Field("map".into()), Nav::Key(n(0))])
        .unwrap()
        .prefix;
    let i0 = durable::dynpath::resolve(
        &schema,
        None,
        &[Nav::Field("list".into()), Nav::Index(0)],
    )
    .unwrap()
    .prefix;
    assert!(!overlap(&f0, &k0), "field-id 0 vs map-key 0: {f0:?} vs {k0:?}");
    assert!(!overlap(&f0, &i0), "field-id 0 vs list-idx 0: {f0:?} vs {i0:?}");
    assert!(!overlap(&k0, &i0), "map-key 0 vs list-idx 0: {k0:?} vs {i0:?}");

    // …and hostile key bytes (empty, 0x00/0x01-led, long) stay disjoint.
    let mut keys = vec![n(0), n(1), n(127), n(256), t(""), t("a"), t("ab")];
    keys.push(Value::Bytes(vec![0x00]));
    keys.push(Value::Bytes(vec![0x01, 0x02]));
    let mut ps = Vec::new();
    for k in &keys {
        ps.push(
            durable::dynpath::resolve(&schema, None, &[Nav::Field("map".into()), Nav::Key(k.clone())])
                .unwrap()
                .prefix,
        );
    }
    for (i, a) in ps.iter().enumerate() {
        for b in &ps[i + 1..] {
            assert!(!overlap(a, b), "map keys overlap: {a:?} vs {b:?}");
        }
    }
}

#[test]
fn delete_prefix_hits_parent_and_child_not_sibling() {
    let (_dir, db) = open();
    let schema = shapes()[0].1.clone();
    let w = delete(
        &schema,
        None,
        &navs_for(&schema, &[t("users"), t("alice")]).unwrap(),
    )
    .unwrap();
    let ws = write_set(std::slice::from_ref(&w));
    assert_eq!(ws.len(), 1);

    // parent record and the deleted subtree overlap the range…
    for steps in [
        vec![t("users")],
        vec![t("users"), t("alice")],
        vec![t("users"), t("alice"), t("name")],
    ] {
        let navs = navs_for(&schema, &steps).unwrap();
        assert!(
            invalidated(&schema, &navs, std::slice::from_ref(&w)),
            "delete must hit {steps:?}"
        );
    }
    // …a sibling subtree does not.
    let navs = navs_for(&schema, &[t("users"), t("bob")]).unwrap();
    assert!(!invalidated(&schema, &navs, std::slice::from_ref(&w)));
    let navs = navs_for(&schema, &[t("title")]).unwrap();
    assert!(!invalidated(&schema, &navs, std::slice::from_ref(&w)));
    let _ = db;
}

#[test]
fn sum_merge_overlaps_its_point_read() {
    let schema = shapes()[0].1.clone();
    let navs = navs_for(&schema, &[t("total")]).unwrap();
    let w = add(&schema, None, &navs, &n(1)).unwrap();
    assert!(invalidated(&schema, &navs, std::slice::from_ref(&w)));
    let other = navs_for(&schema, &[t("title")]).unwrap();
    assert!(!invalidated(&schema, &other, std::slice::from_ref(&w)));
}

#[test]
fn missing_key_creation_overlaps() {
    // A read of an absent key answers Null from the location, not the
    // storage: the location prefix is the read set, so creating it hits.
    let (_dir, db) = open();
    let schema = shapes()[0].1.clone();
    let navs = navs_for(&schema, &[t("users"), t("nobody")]).unwrap();
    let q = Query::new(navs.clone());
    assert_eq!(subtree(&db, &schema, &q).unwrap(), Value::Null);
    let w = put(
        &schema,
        None,
        &navs,
        &Value::Map(vec![(t("name"), t("zed"))]),
    )
    .unwrap();
    let name_navs = navs_for(&schema, &[t("users"), t("nobody"), t("name")]).unwrap();
    assert!(invalidated(&schema, &name_navs, &w));
    db.apply(&w, Durability::DisableWal).unwrap();
    assert_eq!(
        subtree(&db, &schema, &q).unwrap(),
        Value::Map(vec![(t("name"), t("zed")), (t("age"), Value::Null)])
    );
}

#[test]
fn list_push_overlaps_point_and_range_reads() {
    // A push resolves under the list prefix (element key + len meta), so
    // every read at or under the list overlaps. Point reads of untouched
    // indices overlap too — sound, imprecise, and counted as such.
    let (_dir, db) = open();
    let schema = shapes()[1].1.clone();
    let at = |steps: &[Value]| navs_for(&schema, steps).unwrap();
    let w = push(&schema, None, &at(&[t("feed")]), &t("hello")).unwrap();
    db.apply(std::slice::from_ref(&w), Durability::DisableWal).unwrap();

    assert!(invalidated(&schema, &at(&[t("feed")]), std::slice::from_ref(&w)));
    assert!(invalidated(
        &schema,
        &at(&[t("feed"), n(0)]),
        std::slice::from_ref(&w)
    ));
    // sibling collection untouched.
    assert!(!invalidated(
        &schema,
        &at(&[t("scores"), t("a")]),
        std::slice::from_ref(&w)
    ));
    // deque in the SAME schema lives under a disjoint prefix.
    let mixed = shapes()[2].1.clone();
    let dw = push(
        &mixed,
        None,
        &navs_for(&mixed, &[t("dq")]).unwrap(),
        &n(9),
    )
    .unwrap();
    let ra = navs_for(&mixed, &[t("r"), t("a")]).unwrap();
    assert!(!invalidated(&mixed, &ra, std::slice::from_ref(&dw)));
}

// ---------------------------------------------------------------------------
// Fuzz: changed ⟹ overlap, over schemas × writes × reads
// ---------------------------------------------------------------------------

struct Rng(u64);

impl Rng {
    fn next(&mut self, n: u64) -> u64 {
        // xorshift64*: deterministic per seed, no new deps.
        let mut x = self.0 | 1;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        (x.wrapping_mul(0x2545F4914F6CDD1D) >> 11) % n.max(1)
    }
}

fn gen_scalar(r: &mut Rng) -> Value {
    match r.next(6) {
        0 => n(r.next(6) as i64),
        1 => t(["", "a", "b", "x"][r.next(4) as usize]),
        2 => Value::Bool(r.next(2) == 0),
        3 => Value::Null,
        4 => Value::Array((0..r.next(3)).map(|_| gen_scalar_leaf(r)).collect()),
        _ => Value::Map(
            (0..r.next(3))
                .map(|i| (t(["p", "q"][i as usize % 2]), gen_scalar_leaf(r)))
                .collect(),
        ),
    }
}

fn gen_scalar_leaf(r: &mut Rng) -> Value {
    match r.next(4) {
        0 => n(r.next(6) as i64),
        1 => t(["", "a", "b"][r.next(3) as usize]),
        2 => Value::Bool(r.next(2) == 0),
        _ => Value::Null,
    }
}

fn key_pool(r: &mut Rng) -> Value {
    match r.next(6) {
        0 => n(r.next(4) as i64),
        1 => t(["a", "b", ""][r.next(3) as usize]),
        2 => Value::Bool(true),
        3 => n(127),
        4 => n(128),
        _ => t("user"),
    }
}

/// Random walk down the schema. `for_write` stops at put/add/push/del
/// targets; reads may descend into leaf interiors (in-memory).
fn walk(r: &mut Rng, shape: &Shape, steps: &mut Vec<Value>, for_write: bool) {
    let mut cur = shape;
    for _ in 0..4 {
        match cur {
            Shape::Record { fields } => {
                if fields.is_empty() || (for_write && r.next(3) == 0) {
                    return;
                }
                let i = r.next(fields.len() as u64) as usize;
                steps.push(t(&fields[i].0));
                cur = &fields[i].1;
            }
            Shape::Map { of } => {
                if for_write && r.next(3) == 0 {
                    return;
                }
                steps.push(key_pool(r));
                cur = of;
            }
            Shape::List { of } | Shape::Deque { of } => {
                if for_write && r.next(3) == 0 {
                    return;
                }
                steps.push(n(r.next(5) as i64));
                cur = of;
            }
            Shape::Leaf | Shape::Sum => return,
        }
    }
}

fn shape_at<'a>(shape: &'a Shape, navs: &[Nav]) -> Option<&'a Shape> {
    let mut cur = shape;
    for nav in navs {
        match (cur, nav) {
            (Shape::Record { fields: fs }, Nav::Field(name)) => {
                cur = &fs.iter().find(|(n, _)| n == name)?.1;
            }
            (Shape::Map { of }, Nav::Key(_)) | (Shape::Map { of }, Nav::Field(_)) => cur = of,
            (Shape::List { of }, Nav::Index(_))
            | (Shape::Deque { of }, Nav::Index(_))
            | (Shape::List { of }, Nav::Key(_))
            | (Shape::Deque { of }, Nav::Key(_)) => cur = of,
            _ => return None,
        }
    }
    Some(cur)
}

fn run_fuzz(seed: u64) -> (u64, u64, u64) {
    let (_dir, db) = open();
    let schemas = shapes();
    let mut changed_n = 0u64;
    let mut overlap_n = 0u64;
    let mut iters = 0u64;

    for round in 0..300 {
        let mut r = Rng(seed.wrapping_add((round as u64).wrapping_mul(0x9E3779B97F4A7C15)));
        let schema = &schemas[r.next(schemas.len() as u64) as usize].1;

        // ---- random write ----
        let mut wsteps = Vec::new();
        walk(&mut r, schema, &mut wsteps, true);
        let wnavs = match navs_for(schema, &wsteps) {
            Ok(n) => n,
            Err(_) => continue,
        };
        let target = match shape_at(schema, &wnavs) {
            Some(s) => s,
            None => continue,
        };
        let writes: Vec<durable::Write> = match r.next(5) {
            0 => match target {
                Shape::Leaf => match put(schema, None, &wnavs, &gen_scalar(&mut r)) {
                    Ok(w) => w,
                    Err(_) => continue,
                },
                Shape::Sum => match put(schema, None, &wnavs, &n(r.next(10) as i64)) {
                    Ok(w) => w,
                    Err(_) => continue,
                },
                Shape::Record { .. } => {
                    // rebuild from the target's own fields so puts expand.
                    let Shape::Record { fields: fs } = target else { continue };
                    match put(schema, None, &wnavs, &gen_put_record(&mut r, fs)) {
                        Ok(w) => w,
                        Err(_) => continue,
                    }
                }
                _ => continue,
            },
            1 => match target {
                Shape::Sum => match add(schema, None, &wnavs, &n(1 + r.next(5) as i64)) {
                    Ok(w) => vec![w],
                    Err(_) => continue,
                },
                _ => continue,
            },
            2 => match push(schema, None, &wnavs, &gen_scalar_leaf(&mut r)) {
                Ok(w) => vec![w],
                Err(_) => continue,
            },
            3 => match delete(schema, None, &wnavs) {
                Ok(w) => vec![w],
                Err(_) => continue,
            },
            _ => match clear(schema, None, &wnavs) {
                Ok(w) => vec![w],
                Err(_) => continue,
            },
        };
        if writes.is_empty() {
            continue;
        }

        // ---- random read: half the time, read AT the write (an
        // ancestor prefix of it, point or collecting) so the pair
        // interacts; otherwise wander independently for the negative
        // side. Uncorrelated fuzzing is vacuous — pairs that never
        // touch prove nothing.
        let rnavs: Vec<Nav> = if r.next(2) == 0 && !wnavs.is_empty() {
            let cut = r.next(wnavs.len() as u64 + 1) as usize;
            let mut v: Vec<Nav> = wnavs[..cut.min(wnavs.len())].to_vec();
            if r.next(3) == 0 {
                if let Some(s) = shape_at(schema, &v) {
                    match s {
                        Shape::Map { .. } | Shape::List { .. } | Shape::Deque { .. } => {
                            v.push(match r.next(3) {
                                0 => Nav::All,
                                1 => Nav::Entries,
                                _ => Nav::Values,
                            });
                        }
                        _ => {}
                    }
                }
            }
            v
        } else {
            let mut rsteps = Vec::new();
            walk(&mut r, schema, &mut rsteps, false);
            let mut v = match navs_for(schema, &rsteps) {
                Ok(n) => n,
                Err(_) => continue,
            };
            if r.next(3) == 0 {
                plant_collector(&mut r, schema, &mut v);
            }
            v
        };
        let query = Query::new(rnavs.clone());
        let collecting = query.is_collecting();
        let before = match read_eval(&db, schema, &query, collecting) {
            Some(v) => v,
            None => continue,
        };

        if db.apply(&writes, Durability::DisableWal).is_err() {
            continue;
        }
        let after = match read_eval(&db, schema, &query, collecting) {
            Some(v) => v,
            None => continue,
        };
        iters += 1;

        let changed = before != after;
        let hit = invalidated(schema, &rnavs, &writes);
        if changed {
            changed_n += 1;
            assert!(
                hit,
                "MISSED INVALIDATION\nschema steps write={wsteps:?} read navs={rnavs:?}\nbefore={before:?}\nafter={after:?}"
            );
        }
        if hit {
            overlap_n += 1;
        }
        let _ = v_placeholder();
    }
    (iters, changed_n, overlap_n)
}

fn v_placeholder() {}

fn gen_put_record(r: &mut Rng, fs: &[(String, Shape)]) -> Value {
    let mut pairs = Vec::new();
    for (name, _) in fs {
        if r.next(2) == 0 {
            pairs.push((t(name), gen_scalar_leaf(r)));
        }
    }
    Value::Map(pairs)
}

/// Plant a collecting nav at the first collection node of a nav run
/// (independent-side reads).
fn plant_collector(r: &mut Rng, schema: &Shape, rnavs: &mut Vec<Nav>) {
    let mut cut = None;
    let mut cur = schema;
    for (i, nav) in rnavs.iter().enumerate() {
        match (cur, nav) {
            (Shape::Map { .. } | Shape::List { .. } | Shape::Deque { .. }, _) => {
                cut = Some(i);
                break;
            }
            (s, n) => match shape_at_one(s, n) {
                Some(next) => cur = next,
                None => break,
            },
        }
    }
    if let Some(i) = cut {
        let coll = match r.next(7) {
            0 => Nav::All,
            1 => Nav::Entries,
            2 => Nav::Keys,
            3 => Nav::Values,
            4 => Nav::First,
            5 => Nav::Slice {
                start: Some(r.next(3)),
                end: if r.next(2) == 0 {
                    Some(1 + r.next(3))
                } else {
                    None
                },
            },
            _ => Nav::Where(durable::Predicate::Eq(
                durable::Expr::Field("name".into()),
                durable::Expr::Lit(t("a")),
            )),
        };
        rnavs.truncate(i + 1);
        rnavs.push(coll);
    }
}

fn shape_at_one<'a>(shape: &'a Shape, nav: &Nav) -> Option<&'a Shape> {
    match (shape, nav) {
        (Shape::Record { fields: fs }, Nav::Field(name)) => {
            fs.iter().find(|(n, _)| n == name).map(|(_, s)| s)
        }
        (Shape::Map { of }, Nav::Key(_)) | (Shape::Map { of }, Nav::Field(_)) => Some(of),
        (Shape::List { of }, Nav::Index(_))
        | (Shape::Deque { of }, Nav::Index(_))
        | (Shape::List { of }, Nav::Key(_))
        | (Shape::Deque { of }, Nav::Key(_)) => Some(of),
        (Shape::Leaf, _) => Some(shape),
        _ => None,
    }
}

fn read_eval(db: &Db, schema: &Shape, query: &Query, collecting: bool) -> Option<String> {
    let v = if collecting {
        select(db, schema, query).ok().map(|vs| format!("{vs:?}"))?
    } else {
        // rotate terminals across seeds for coverage
        match query.navs.len() % 3 {
            0 => subtree(db, schema, query).ok().map(|v| format!("{v:?}"))?,
            1 => one(db, schema, query)
                .ok()
                .map(|v| format!("{v:?}"))?,
            _ => has(db, schema, query).ok().map(|v| format!("{v:?}"))?,
        }
    };
    Some(v)
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 64, .. ProptestConfig::default() })]
    #[test]
    fn courier_overlap_sound(seed in any::<u64>()) {
        let (iters, changed, overlapped) = run_fuzz(seed);
        // Every seed must exercise real changes, or the fuzz is vacuous.
        prop_assert!(iters > 40, "too few iterations: {iters}");
        prop_assert!(changed > 0, "no read ever changed — fuzz writes nothing?");
        // …and overlap must fire (else the courier never repaints).
        prop_assert!(overlapped > 0, "overlap never fired — read sets broken?");
    }
}

#[test]
fn root_subscription_matches_everything() {
    // The empty prefix is every key's prefix: a root read overlaps all
    // writes. Trivial, but load-bearing — the courier's match-all.
    assert!(overlap(&[], &[0x01]));
    assert!(overlap(&[0x01], &[]));
    assert!(overlap(&[], &[]));
}
