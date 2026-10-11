//! Invalidation lab: is "read-prefix ∩ write-prefix" a sound way to decide
//! which views need a repaint after a commit?
//!
//! Ground truth: run the query before and after each write batch and compare
//! results. A strategy MISSES when the result changed but the strategy did
//! not flag it (a stale screen), and FALSE-POSITIVES when it flagged a view
//! whose result did not change (a wasted repaint).
//!
//!   cargo test -p durable --test invalidation_lab -- --nocapture
//!
//! Parts 1–3 attack query semantics (collecting views, one-way checks,
//! registration races) on the `Lab` schema. Part 4 attacks the codec and
//! layout layer (sibling disjointness at every varint width, tag confusion,
//! hostile keys) plus a schema-walk fuzz over four shapes — ported from the
//! courier overlap suite, which this file absorbs and replaces.

use ciborium::value::Integer;
use ciborium::Value;
use durable::{dynpath, navs_for, query, Db, Describe, Deque, Durability, Durable, Expr, Leaf, List, Map, Nav, Op, Predicate, Query, Shape, Sum, Write};
use proptest::prelude::*;
use tempfile::TempDir;

#[derive(Durable)]
#[allow(dead_code)]
struct User {
    name: Leaf<String>,
    banned: Leaf<bool>,
    profile: Leaf<Value>, // a CBOR blob: { name, bio }
}

#[derive(Durable)]
#[allow(dead_code)]
struct Room {
    messages: List<Leaf<Value>>,
}

#[derive(Durable)]
#[allow(dead_code)]
struct Lab {
    users: Map<String, User>,
    rooms: Map<String, Room>,
    queue: Deque<Leaf<Value>>,
    scores: Map<String, Sum<i64>>,
    count: Sum<i64>,
    board: Map<String, Leaf<Value>>,
}

// ── tiny helpers ───────────────────────────────────────────────────────────

fn t(s: &str) -> Value {
    Value::Text(s.into())
}
fn i(n: i64) -> Value {
    Value::Integer(Integer::from(n))
}
fn f(s: &str) -> Nav {
    Nav::Field(s.into())
}
fn k(s: &str) -> Nav {
    Nav::Key(t(s))
}
fn map(pairs: Vec<(&str, Value)>) -> Value {
    Value::Map(pairs.into_iter().map(|(a, b)| (t(a), b)).collect())
}

/// Smallest key strictly greater than every key starting with `p`.
fn upper(p: &[u8]) -> Option<Vec<u8>> {
    let mut e = p.to_vec();
    while let Some(last) = e.last_mut() {
        if *last != 0xff {
            *last += 1;
            return Some(e);
        }
        e.pop();
    }
    None
}

/// Half-open key interval [lo, hi); hi = None means "to the end".
#[derive(Clone, Debug)]
struct Range {
    lo: Vec<u8>,
    hi: Option<Vec<u8>>,
}

impl Range {
    fn prefix(p: &[u8]) -> Range {
        Range { lo: p.to_vec(), hi: upper(p) }
    }
    fn point(key: &[u8]) -> Range {
        let mut hi = key.to_vec();
        hi.push(0);
        Range { lo: key.to_vec(), hi: Some(hi) }
    }
    fn overlaps(&self, o: &Range) -> bool {
        let a_before_o_end = o.hi.as_ref().map_or(true, |h| self.lo < *h);
        let o_before_a_end = self.hi.as_ref().map_or(true, |h| o.lo < *h);
        a_before_o_end && o_before_a_end
    }
}

/// What a committed write touched, as key ranges.
fn write_ranges(w: &Write) -> Range {
    match w.op() {
        Op::Put { key, .. } | Op::Delete { key } | Op::Merge { key, .. } => Range::point(key),
        Op::DeletePrefix { prefix }
        | Op::ListPush { prefix, .. }
        | Op::DequePushBack { prefix, .. }
        | Op::DequePushFront { prefix, .. } => Range::prefix(prefix),
    }
}

/// The ramalite proposal: register the longest *point* prefix of the query's
/// path (stop at the first collecting/positional nav, at a deque index, or on
/// entering a leaf's interior), as the range of everything under it.
fn read_prefix(schema: &Shape, navs: &[Nav]) -> Vec<u8> {
    let mut best = Vec::new();
    for n in 0..navs.len() {
        match &navs[n] {
            Nav::Field(_) | Nav::Key(_) | Nav::Index(_) => {}
            _ => break,
        }
        match dynpath::resolve(schema, None, &navs[..=n]) {
            Ok(loc) => best = loc.prefix,
            Err(_) => break,
        }
    }
    best
}

/// Strategy A — prefix registration, interval-overlap check (both directions).
fn flag_bidir(read: &Range, writes: &[Write]) -> bool {
    writes.iter().any(|w| write_ranges(w).overlaps(read))
}

/// Strategy B — prefix registration, but the naive one-way check:
/// "does the written key live under the read prefix?"
fn flag_oneway(read_p: &[u8], writes: &[Write]) -> bool {
    writes.iter().any(|w| write_ranges(w).lo.starts_with(read_p))
}

fn eval(db: &Db, schema: &Shape, q: &[Nav]) -> Value {
    let query = Query::new(q.to_vec());
    if query.is_collecting() {
        Value::Array(query::select(db, schema, &query).unwrap())
    } else {
        query::subtree(db, schema, &query).unwrap()
    }
}

fn open() -> (TempDir, Db) {
    let dir = TempDir::new().unwrap();
    let db = Db::open(dir.path()).unwrap();
    (dir, db)
}

fn apply(db: &Db, ws: &[Write]) {
    db.apply(ws, Durability::DisableWal).unwrap();
}

// ── write builders over the dynamic surface ────────────────────────────────

fn put(s: &Shape, navs: Vec<Nav>, v: Value) -> Vec<Write> {
    dynpath::put(s, None, &navs, &v).unwrap()
}
fn del(s: &Shape, navs: Vec<Nav>) -> Write {
    dynpath::delete(s, None, &navs).unwrap()
}
fn add(s: &Shape, navs: Vec<Nav>, d: i64) -> Write {
    dynpath::add(s, None, &navs, &i(d)).unwrap()
}
fn push(s: &Shape, navs: Vec<Nav>, v: Value) -> Write {
    dynpath::push(s, None, &navs, &v).unwrap()
}
fn user_rec(name: &str, banned: bool) -> Value {
    map(vec![
        ("name", t(name)),
        ("banned", Value::Bool(banned)),
        ("profile", map(vec![("name", t(name)), ("bio", t("hi"))])),
    ])
}

// ── part 1: named edge cases ───────────────────────────────────────────────

struct Case {
    name: &'static str,
    setup: Vec<Write>,
    query: Vec<Nav>,
    /// what a "register only the rows I actually saw" strategy would register
    narrow: Option<Vec<Range>>,
    write: Vec<Write>,
}

fn run_case(schema: &Shape, c: Case) -> (bool, bool, bool, Option<bool>) {
    let (_d, db) = open();
    apply(&db, &c.setup);
    let before = eval(&db, schema, &c.query);
    let p = read_prefix(schema, &c.query);
    let a = flag_bidir(&Range::prefix(&p), &c.write);
    let b = flag_oneway(&p, &c.write);
    let n = c.narrow.as_ref().map(|rs| {
        rs.iter().any(|r| c.write.iter().any(|w| write_ranges(w).overlaps(r)))
    });
    apply(&db, &c.write);
    let after = eval(&db, schema, &c.query);
    (before != after, a, b, n)
}

fn verdict(changed: bool, flagged: bool) -> &'static str {
    match (changed, flagged) {
        (true, true) => "ok",
        (false, false) => "ok",
        (true, false) => "MISS",
        (false, true) => "extra",
    }
}

#[test]
fn named_edge_cases() {
    let s = Lab::shape();
    let root = Lab::root();
    let key_of = |navs: Vec<Nav>| dynpath::resolve(&s, None, &navs).unwrap().prefix;
    let bob_banned_key = key_of(vec![f("users"), k("bob"), f("banned")]);
    let q_prefix = key_of(vec![f("queue")]);

    let base_users = {
        let mut w = put(&s, vec![f("users"), k("bob")], user_rec("bob", false));
        w.extend(put(&s, vec![f("users"), k("bobby")], user_rec("bobby", false)));
        w.extend(put(&s, vec![f("users"), k("carol")], user_rec("carol", false)));
        w
    };

    let cases = vec![
        Case {
            name: "delete parent record, view watches a child field",
            setup: base_users.clone(),
            query: vec![f("users"), k("bob"), f("banned")],
            narrow: Some(vec![Range::point(&bob_banned_key)]),
            write: vec![del(&s, vec![f("users"), k("bob")])],
        },
        Case {
            name: "clear the whole map, view watches one entry's field",
            setup: base_users.clone(),
            query: vec![f("users"), k("carol"), f("name")],
            narrow: None,
            write: vec![del(&s, vec![f("users")])],
        },
        Case {
            name: "sibling with a longer name (bob vs bobby)",
            setup: base_users.clone(),
            query: vec![f("users"), k("bob")],
            narrow: None,
            write: put(&s, vec![f("users"), k("bobby"), f("banned")], Value::Bool(true)),
        },
        Case {
            name: "where banned==true, a NEW user becomes banned",
            setup: base_users.clone(),
            query: vec![
                f("users"),
                Nav::Where(Predicate::Eq(Expr::Field("banned".into()), Expr::Lit(Value::Bool(true)))),
                f("name"),
            ],
            // nobody is banned yet: the narrow strategy registered nothing
            narrow: Some(vec![]),
            write: put(&s, vec![f("users"), k("carol"), f("banned")], Value::Bool(true)),
        },
        Case {
            name: "first entry of a map, a key that sorts earlier is inserted",
            setup: base_users.clone(),
            query: vec![f("users"), Nav::First, f("name")],
            narrow: Some(vec![Range::prefix(&key_of(vec![f("users"), k("bob")]))]),
            write: put(&s, vec![f("users"), k("al")], user_rec("al", false)),
        },
        Case {
            name: "slice(0,2) of a map, insert sorts into the window",
            setup: base_users.clone(),
            query: vec![f("users"), Nav::Slice { start: Some(0), end: Some(2) }, f("name")],
            narrow: Some(vec![
                Range::prefix(&key_of(vec![f("users"), k("bob")])),
                Range::prefix(&key_of(vec![f("users"), k("bobby")])),
            ]),
            write: put(&s, vec![f("users"), k("al")], user_rec("al", false)),
        },
        Case {
            name: "last message of a room, a message is pushed",
            setup: vec![
                push(&s, vec![f("rooms"), k("lobby"), f("messages")], t("one")),
                push(&s, vec![f("rooms"), k("lobby"), f("messages")], t("two")),
            ],
            query: vec![f("rooms"), k("lobby"), f("messages"), Nav::Last],
            narrow: Some(vec![Range::point(&key_of(vec![
                f("rooms"), k("lobby"), f("messages"), Nav::Index(1),
            ]))]),
            write: vec![push(&s, vec![f("rooms"), k("lobby"), f("messages")], t("three"))],
        },
        Case {
            name: "queue[0] (deque), push_front shifts what index 0 means",
            setup: vec![root.queue().push_back_op(&t("a"))],
            query: vec![f("queue"), Nav::Index(0)],
            // naive: element key for logical 0 = physical slot 0, plus nothing else
            narrow: Some(vec![Range::point(&{
                let mut kk = q_prefix.clone();
                kk.push(0x01);
                kk.push(8);
                kk.extend_from_slice(&((0i64 as u64) ^ (1u64 << 63)).to_be_bytes());
                kk
            })]),
            write: vec![root.queue().push_front_op(&t("z"))],
        },
        Case {
            name: "field inside a leaf blob, a different field of the blob changes",
            setup: base_users.clone(),
            query: vec![f("users"), k("bob"), f("profile"), f("name")],
            narrow: None,
            write: put(&s, vec![f("users"), k("bob"), f("profile")],
                map(vec![("name", t("bob")), ("bio", t("new bio"))])),
        },
        Case {
            name: "sum, add(0)",
            setup: vec![add(&s, vec![f("count")], 5)],
            query: vec![f("count")],
            narrow: None,
            write: vec![add(&s, vec![f("count")], 0)],
        },
        Case {
            name: "put the same value again",
            setup: base_users.clone(),
            query: vec![f("users"), k("bob"), f("name")],
            narrow: None,
            write: put(&s, vec![f("users"), k("bob"), f("name")], t("bob")),
        },
        Case {
            name: "view of missing room, first message creates it",
            setup: vec![],
            query: vec![f("rooms"), k("new"), f("messages")],
            narrow: Some(vec![]),
            write: vec![push(&s, vec![f("rooms"), k("new"), f("messages")], t("hi"))],
        },
        Case {
            name: "record put omits a field (writes it as null)",
            setup: base_users.clone(),
            query: vec![f("users"), k("carol"), f("banned")],
            narrow: None,
            write: put(&s, vec![f("users"), k("carol")], map(vec![("name", t("carol"))])),
        },
        Case {
            name: "keys of a map, a value (not a key) changes",
            setup: base_users.clone(),
            query: vec![f("users"), Nav::Keys],
            narrow: None,
            write: put(&s, vec![f("users"), k("bob"), f("name")], t("robert")),
        },
    ];

    println!();
    println!("{:<62} {:>7}  {:>14}  {:>14}  {:>12}", "case", "changed", "prefix+overlap", "prefix+oneway", "narrow rows");
    let mut misses_a = 0;
    for c in cases {
        let name = c.name;
        let (changed, a, b, n) = run_case(&s, c);
        if changed && !a {
            misses_a += 1;
        }
        println!(
            "{:<62} {:>7}  {:>14}  {:>14}  {:>12}",
            name,
            changed,
            verdict(changed, a),
            verdict(changed, b),
            n.map(|n| verdict(changed, n)).unwrap_or("-"),
        );
    }
    assert_eq!(misses_a, 0, "prefix+overlap missed a change");
}

// ── part 2: randomized ─────────────────────────────────────────────────────

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        &xs[(self.next() % xs.len() as u64) as usize]
    }
    fn n(&mut self, m: u64) -> u64 {
        self.next() % m
    }
}

const NAMES: [&str; 5] = ["bob", "bobby", "al", "carol", "zed"];
const ROOMS: [&str; 3] = ["lobby", "dev", "x"];

fn random_write(s: &Shape, r: &mut Rng) -> Vec<Write> {
    let u = *r.pick(&NAMES);
    let room = *r.pick(&ROOMS);
    let root = Lab::root();
    match r.n(20) {
        0 => put(s, vec![f("users"), k(u), f("name")], t(r.pick(&NAMES))),
        1 => put(s, vec![f("users"), k(u), f("banned")], Value::Bool(r.n(2) == 0)),
        2 => put(s, vec![f("users"), k(u), f("profile")],
            map(vec![("name", t(u)), ("bio", t(r.pick(&["hi", "yo"])))])),
        3 => put(s, vec![f("users"), k(u)], user_rec(u, r.n(2) == 0)),
        4 => put(s, vec![f("users"), k(u)], map(vec![("name", t(u))])),
        5 => vec![del(s, vec![f("users"), k(u)])],
        6 => vec![del(s, vec![f("users"), k(u), f("banned")])],
        7 => if r.n(6) == 0 { vec![del(s, vec![f("users")])] } else { vec![] },
        8 | 9 => vec![push(s, vec![f("rooms"), k(room), f("messages")], t(r.pick(&NAMES)))],
        10 => vec![del(s, vec![f("rooms"), k(room)])],
        11 => vec![root.queue().push_back_op(&t(r.pick(&NAMES)))],
        12 => vec![root.queue().push_front_op(&t(r.pick(&NAMES)))],
        13 => if r.n(4) == 0 { vec![del(s, vec![f("queue")])] } else { vec![] },
        14 => vec![add(s, vec![f("scores"), k(u)], r.n(3) as i64)],
        15 => put(s, vec![f("scores"), k(u)], i(r.n(3) as i64)),
        16 => vec![add(s, vec![f("count")], r.n(2) as i64)],
        17 => put(s, vec![f("board"), k(u)], t(r.pick(&["a", "b"]))),
        18 => vec![del(s, vec![f("board"), k(u)])],
        _ => vec![del(s, vec![f("board")])],
    }
}

fn random_query(r: &mut Rng) -> Vec<Nav> {
    let u = *r.pick(&NAMES);
    let room = *r.pick(&ROOMS);
    let banned_true = Nav::Where(Predicate::Eq(Expr::Field("banned".into()), Expr::Lit(Value::Bool(true))));
    match r.n(22) {
        0 => vec![f("users"), k(u), f("name")],
        1 => vec![f("users"), k(u), f("banned")],
        2 => vec![f("users"), k(u), f("profile")],
        3 => vec![f("users"), k(u), f("profile"), f("bio")],
        4 => vec![f("users"), k(u)],
        5 => vec![f("users"), Nav::All, f("name")],
        6 => vec![f("users"), banned_true, f("name")],
        7 => vec![f("users"), Nav::Keys],
        8 => vec![f("users"), Nav::Slice { start: Some(0), end: Some(2) }, f("name")],
        9 => vec![f("users"), Nav::First, f("name")],
        10 => vec![f("users"), Nav::Last, f("banned")],
        11 => vec![f("rooms"), k(room), f("messages")],
        12 => vec![f("rooms"), k(room), f("messages"), Nav::Last],
        13 => vec![f("rooms"), k(room), f("messages"), Nav::Slice { start: Some(1), end: None }],
        14 => vec![f("queue"), Nav::Index(0)],
        15 => vec![f("queue"), Nav::Values],
        16 => vec![f("queue"), Nav::Last],
        17 => vec![f("scores"), k(u)],
        18 => vec![f("scores"), Nav::Entries],
        19 => vec![f("count")],
        20 => vec![f("board"), k(u)],
        _ => vec![f("board"), Nav::Keys],
    }
}

#[test]
fn randomized_soundness() {
    let s = Lab::shape();
    let mut totals = [(0u64, 0u64, 0u64); 2]; // (changed&&miss, unchanged&&flag, checks)
    let mut changed_total = 0u64;
    let mut unchanged_total = 0u64;
    let mut example_misses_b: Vec<String> = Vec::new();

    for seed in 1..=40u64 {
        let mut r = Rng(0x9E3779B97F4A7C15 ^ (seed * 7919));
        let (_d, db) = open();
        for _ in 0..40 {
            let w = random_write(&s, &mut r);
            apply(&db, &w);
        }
        for _ in 0..500 {
            let q = random_query(&mut r);
            let mut batch = Vec::new();
            for _ in 0..(1 + r.n(3)) {
                batch.extend(random_write(&s, &mut r));
            }
            let before = eval(&db, &s, &q);
            let p = read_prefix(&s, &q);
            let a = flag_bidir(&Range::prefix(&p), &batch);
            let b = flag_oneway(&p, &batch);
            apply(&db, &batch);
            let after = eval(&db, &s, &q);
            let changed = before != after;
            if changed { changed_total += 1 } else { unchanged_total += 1 }
            for (idx, flag) in [a, b].into_iter().enumerate() {
                totals[idx].2 += 1;
                if changed && !flag {
                    totals[idx].0 += 1;
                    if idx == 1 && example_misses_b.len() < 3 {
                        example_misses_b.push(format!("{:?}  <-  {:?}", q, batch.iter().map(|w| format!("{:?}", std::mem::discriminant(w.op()))).collect::<Vec<_>>()));
                    }
                }
                if !changed && flag {
                    totals[idx].1 += 1;
                }
            }
        }
    }
    println!();
    println!("randomized: {} checks, {} results changed, {} did not", totals[0].2, changed_total, unchanged_total);
    for (label, (miss, fp, _)) in ["prefix + interval overlap", "prefix + one-way check"].iter().zip(totals) {
        println!(
            "  {:<28} misses {:>5}   wasted repaints {:>5} ({:.1}% of unchanged)",
            label, miss, fp, 100.0 * fp as f64 / unchanged_total.max(1) as f64
        );
    }
    for m in &example_misses_b {
        println!("  one-way miss e.g. {m}");
    }
    assert_eq!(totals[0].0, 0, "interval overlap must never miss");
}

// ── part 2b: precise on BOTH sides ─────────────────────────────────────────

fn seg(out: &mut Vec<u8>, bytes: &[u8]) {
    let mut n = bytes.len() as u64;
    loop {
        let mut b = (n & 0x7f) as u8;
        n >>= 7;
        if n != 0 { b |= 0x80; }
        out.push(b);
        if n == 0 { break; }
    }
    out.extend_from_slice(bytes);
}

#[test]
fn precise_writes_meet_narrow_reads() {
    // If the commit reports the exact keys a push resolved to (element + len
    // meta) instead of "somewhere under the list", and the view registered
    // only the element it showed, the two precise sides never meet.
    let s = Lab::shape();
    let (_d, db) = open();
    let msgs = vec![f("rooms"), k("lobby"), f("messages")];
    apply(&db, &[push(&s, msgs.clone(), t("one")), push(&s, msgs.clone(), t("two"))]);
    let p = dynpath::resolve(&s, None, &msgs).unwrap().prefix;
    let child = |idx: u64| { let mut kk = p.clone(); kk.push(0x01); seg(&mut kk, &idx.to_be_bytes()); kk };
    let meta_len = { let mut kk = p.clone(); kk.push(0x00); seg(&mut kk, b"len"); kk };

    let mut q = msgs.clone();
    q.push(Nav::Last);
    let before = eval(&db, &s, &q);
    let narrow = vec![Range::point(&child(1))];             // "I showed element 1"
    let exact_writes = vec![Range::point(&child(2)), Range::point(&meta_len)];
    let prefix_read = Range::prefix(&read_prefix(&s, &q));
    apply(&db, &[push(&s, msgs.clone(), t("three"))]);
    let after = eval(&db, &s, &q);
    let narrow_hit = narrow.iter().any(|r| exact_writes.iter().any(|w| w.overlaps(r)));
    let prefix_hit = exact_writes.iter().any(|w| w.overlaps(&prefix_read));
    println!();
    println!("last-of-list after push: changed={} | narrow read x exact write: {} | prefix read x exact write: {}",
        before != after, verdict(before != after, narrow_hit), verdict(before != after, prefix_hit));
    assert!(prefix_hit);
}

// ── part 3: the race between reading and registering ───────────────────────

#[test]
fn register_after_read_race() {
    let s = Lab::shape();
    let (_d, db) = open();
    apply(&db, &put(&s, vec![f("users"), k("bob"), f("banned")], Value::Bool(false)));

    let q = vec![f("users"), k("bob"), f("banned")];
    // 1. the view reads (seq s)
    let shown = eval(&db, &s, &q);
    // 2. another flow commits BEFORE the view's read set is registered
    let w = put(&s, vec![f("users"), k("bob"), f("banned")], Value::Bool(true));
    let registered: Vec<Range> = vec![]; // nothing registered yet: courier checks against nobody
    let flagged_at_commit = registered.iter().any(|r| w.iter().any(|x| write_ranges(x).overlaps(r)));
    apply(&db, &w);
    // 3. now the read set registers — too late; the commit already went by
    let now = eval(&db, &s, &q);
    println!();
    println!("race: screen shows {:?}, store holds {:?}, courier flagged at commit: {}", shown, now, flagged_at_commit);
    println!("      -> stale until the next unrelated write under the same prefix");
    assert_ne!(shown, now);
    assert!(!flagged_at_commit);
}

// ── part 4: codec and layout soundness ─────────────────────────────────────
// Ported from the courier overlap suite (now absorbed here). Parts 1–3 show
// prefix+overlap never misses on query semantics; this part shows the byte
// prefixes themselves are trustworthy: siblings are disjoint at every
// uvarint width, tags keep field-0 / map-key-0 / list-index-0 apart, and a
// schema-walk fuzz asserts `changed ⟹ overlap` across four shapes.
//
// Note on the check: `Range::prefix(a).overlaps(&Range::prefix(b))` below is
// exactly `a.starts_with(b) || b.starts_with(a)` — two prefix ranges either
// nest or are disjoint, so the one-liner *is* interval overlap. The `Range`
// machinery stays because part 1 also needs point keys.

/// The one-line bridge, stated plainly.
fn px_overlap(a: &[u8], b: &[u8]) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

fn cn(i: i64) -> Value {
    Value::Integer(Integer::from(i))
}

fn cshapes() -> Vec<(&'static str, Shape)> {
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
fn codec_siblings_disjoint_at_all_varint_widths() {
    // 17000 fields so field ids cross uvarint widths (0, 1, 127, 128,
    // 16383, 16384): a shorter id encoding must never prefix a longer one.
    let fields: Vec<(String, Shape)> =
        (0..17000).map(|ii| (format!("f{ii}"), Shape::Leaf)).collect();
    let schema = Shape::record(vec![
        ("wide".into(), Shape::record(fields)),
        ("m".into(), Shape::map(Shape::Leaf)),
        ("l".into(), Shape::list(Shape::Leaf)),
    ]);

    let wide = dynpath::resolve(&schema, None, &[Nav::Field("wide".into())])
        .unwrap()
        .prefix;
    let mut kids = Vec::new();
    for ii in [0u64, 1, 5, 126, 127, 128, 129, 1000, 16383, 16384, 16999] {
        let navs = vec![Nav::Field("wide".into()), Nav::Field(format!("f{ii}"))];
        let p = dynpath::resolve(&schema, None, &navs).unwrap().prefix;
        assert!(px_overlap(&p, &wide), "child must overlap its parent");
        kids.push(p);
    }
    for (aa, a) in kids.iter().enumerate() {
        for b in &kids[aa + 1..] {
            assert!(!px_overlap(a, b), "sibling fields overlap: {a:?} vs {b:?}");
        }
    }
    let m = dynpath::resolve(&schema, None, &[Nav::Field("m".into())])
        .unwrap()
        .prefix;
    let l = dynpath::resolve(&schema, None, &[Nav::Field("l".into())])
        .unwrap()
        .prefix;
    assert!(!px_overlap(&wide, &m));
    assert!(!px_overlap(&wide, &l));
    assert!(!px_overlap(&m, &l));
}

#[test]
fn codec_tags_and_hostile_keys_stay_disjoint() {
    // field id 0 encodes as [0x00]; a list index opens with TAG_INDEX
    // (0x00); map key int 0 encodes as CBOR [0x00]. As bare segments they
    // share prefixes — as length-prefixed child keys they must not.
    let schema = Shape::record(vec![
        ("rec".into(), Shape::record(vec![("f0".into(), Shape::Leaf)])),
        ("map".into(), Shape::map(Shape::Leaf)),
        ("list".into(), Shape::list(Shape::Leaf)),
    ]);
    let f0 = dynpath::resolve(
        &schema,
        None,
        &[Nav::Field("rec".into()), Nav::Field("f0".into())],
    )
    .unwrap()
    .prefix;
    let k0 = dynpath::resolve(&schema, None, &[Nav::Field("map".into()), Nav::Key(cn(0))])
        .unwrap()
        .prefix;
    let i0 = dynpath::resolve(
        &schema,
        None,
        &[Nav::Field("list".into()), Nav::Index(0)],
    )
    .unwrap()
    .prefix;
    assert!(!px_overlap(&f0, &k0), "field-id 0 vs map-key 0: {f0:?} vs {k0:?}");
    assert!(!px_overlap(&f0, &i0), "field-id 0 vs list-idx 0: {f0:?} vs {i0:?}");
    assert!(!px_overlap(&k0, &i0), "map-key 0 vs list-idx 0: {k0:?} vs {i0:?}");

    // …and hostile key bytes (empty, 0x00/0x01-led, long) stay disjoint.
    let mut keys = vec![cn(0), cn(1), cn(127), cn(256), t(""), t("a"), t("ab")];
    keys.push(Value::Bytes(vec![0x00]));
    keys.push(Value::Bytes(vec![0x01, 0x02]));
    let mut ps = Vec::new();
    for kk in &keys {
        ps.push(
            dynpath::resolve(&schema, None, &[Nav::Field("map".into()), Nav::Key(kk.clone())])
                .unwrap()
                .prefix,
        );
    }
    for (aa, a) in ps.iter().enumerate() {
        for b in &ps[aa + 1..] {
            assert!(!px_overlap(a, b), "map keys overlap: {a:?} vs {b:?}");
        }
    }
}

#[test]
fn codec_delete_prefix_hits_parent_not_sibling() {
    let schema = cshapes()[0].1.clone();
    let w = dynpath::delete(
        &schema,
        None,
        &navs_for(&schema, &[t("users"), t("alice")]).unwrap(),
    )
    .unwrap();
    let ws = std::slice::from_ref(&w);
    let hit = |steps: &[Value]| {
        let navs = navs_for(&schema, steps).unwrap();
        flag_bidir(&Range::prefix(&read_prefix(&schema, &navs)), ws)
    };
    for steps in [
        vec![t("users")],
        vec![t("users"), t("alice")],
        vec![t("users"), t("alice"), t("name")],
    ] {
        assert!(hit(&steps), "delete must hit {steps:?}");
    }
    assert!(!hit(&[t("users"), t("bob")]));
    assert!(!hit(&[t("title")]));
}

#[test]
fn codec_sum_merge_overlaps_its_point_read() {
    let schema = cshapes()[0].1.clone();
    let navs = navs_for(&schema, &[t("total")]).unwrap();
    let w = dynpath::add(&schema, None, &navs, &cn(1)).unwrap();
    let ws = std::slice::from_ref(&w);
    assert!(flag_bidir(&Range::prefix(&read_prefix(&schema, &navs)), ws));
    let other = navs_for(&schema, &[t("title")]).unwrap();
    assert!(!flag_bidir(&Range::prefix(&read_prefix(&schema, &other)), ws));
}

#[test]
fn codec_missing_key_creation_overlaps() {
    // A read of an absent key answers Null from the location, not the
    // storage: the location prefix is the read set, so creating it hits.
    let (_d, db) = open();
    let schema = cshapes()[0].1.clone();
    let navs = navs_for(&schema, &[t("users"), t("nobody")]).unwrap();
    let q = Query::new(navs.clone());
    assert_eq!(query::subtree(&db, &schema, &q).unwrap(), Value::Null);
    let w = dynpath::put(
        &schema,
        None,
        &navs,
        &Value::Map(vec![(t("name"), t("zed"))]),
    )
    .unwrap();
    let name_navs = navs_for(&schema, &[t("users"), t("nobody"), t("name")]).unwrap();
    assert!(flag_bidir(&Range::prefix(&read_prefix(&schema, &name_navs)), &w));
    db.apply(&w, Durability::DisableWal).unwrap();
    assert_eq!(
        query::subtree(&db, &schema, &q).unwrap(),
        Value::Map(vec![(t("name"), t("zed")), (t("age"), Value::Null)])
    );
}

#[test]
fn codec_list_push_overlaps_point_and_range_reads() {
    let (_d, db) = open();
    let schema = cshapes()[1].1.clone();
    let at = |steps: &[Value]| navs_for(&schema, steps).unwrap();
    let w = dynpath::push(&schema, None, &at(&[t("feed")]), &t("hello")).unwrap();
    db.apply(std::slice::from_ref(&w), Durability::DisableWal).unwrap();

    let ws = std::slice::from_ref(&w);
    assert!(flag_bidir(&Range::prefix(&read_prefix(&schema, &at(&[t("feed")]))), ws));
    assert!(flag_bidir(
        &Range::prefix(&read_prefix(&schema, &at(&[t("feed"), cn(0)]))),
        ws
    ));
    assert!(!flag_bidir(
        &Range::prefix(&read_prefix(&schema, &at(&[t("scores"), t("a")]))),
        ws
    ));
    // deque in the SAME schema family lives under a disjoint prefix.
    let mixed = cshapes()[2].1.clone();
    let dw = dynpath::push(
        &mixed,
        None,
        &navs_for(&mixed, &[t("dq")]).unwrap(),
        &cn(9),
    )
    .unwrap();
    let ra = navs_for(&mixed, &[t("r"), t("a")]).unwrap();
    assert!(!flag_bidir(
        &Range::prefix(&read_prefix(&mixed, &ra)),
        std::slice::from_ref(&dw)
    ));
}

#[test]
fn codec_root_matches_everything() {
    // The empty prefix is every key's prefix: a root read overlaps all
    // writes. Trivial, but load-bearing — the match-all subscription.
    assert!(px_overlap(&[], &[0x01]));
    assert!(px_overlap(&[0x01], &[]));
    assert!(px_overlap(&[], &[]));
}

// ── part 4b: schema-walk fuzz over four shapes ─────────────────────────────
// Complements part 2's query-heavy fuzz on `Lab` with shape-heavy coverage:
// random walks down records/maps/lists/deques, half the reads aimed at the
// write so pairs interact (uncorrelated fuzzing proves nothing).

struct CodecRng(u64);

impl CodecRng {
    fn next(&mut self, nn: u64) -> u64 {
        // xorshift64*: deterministic per seed, no new deps.
        let mut x = self.0 | 1;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        (x.wrapping_mul(0x2545F4914F6CDD1D) >> 11) % nn.max(1)
    }
}

fn cgen_scalar_leaf(r: &mut CodecRng) -> Value {
    match r.next(4) {
        0 => cn(r.next(6) as i64),
        1 => t(["", "a", "b"][r.next(3) as usize]),
        2 => Value::Bool(r.next(2) == 0),
        _ => Value::Null,
    }
}

fn cgen_scalar(r: &mut CodecRng) -> Value {
    match r.next(6) {
        0 => cn(r.next(6) as i64),
        1 => t(["", "a", "b", "x"][r.next(4) as usize]),
        2 => Value::Bool(r.next(2) == 0),
        3 => Value::Null,
        4 => Value::Array((0..r.next(3)).map(|_| cgen_scalar_leaf(r)).collect()),
        _ => Value::Map(
            (0..r.next(3))
                .map(|ii| (t(["p", "q"][ii as usize % 2]), cgen_scalar_leaf(r)))
                .collect(),
        ),
    }
}

fn ckey_pool(r: &mut CodecRng) -> Value {
    match r.next(6) {
        0 => cn(r.next(4) as i64),
        1 => t(["a", "b", ""][r.next(3) as usize]),
        2 => Value::Bool(true),
        3 => cn(127),
        4 => cn(128),
        _ => t("user"),
    }
}

/// Random walk down the schema. `for_write` stops at put/add/push/del
/// targets; reads may descend into leaf interiors (in-memory).
fn cwalk(r: &mut CodecRng, shape: &Shape, steps: &mut Vec<Value>, for_write: bool) {
    let mut cur = shape;
    for _ in 0..4 {
        match cur {
            Shape::Record { fields } => {
                if fields.is_empty() || (for_write && r.next(3) == 0) {
                    return;
                }
                let ii = r.next(fields.len() as u64) as usize;
                steps.push(t(&fields[ii].0));
                cur = &fields[ii].1;
            }
            Shape::Map { of } => {
                if for_write && r.next(3) == 0 {
                    return;
                }
                steps.push(ckey_pool(r));
                cur = of;
            }
            Shape::List { of } | Shape::Deque { of } => {
                if for_write && r.next(3) == 0 {
                    return;
                }
                steps.push(cn(r.next(5) as i64));
                cur = of;
            }
            Shape::Leaf | Shape::Sum => return,
        }
    }
}

fn cshape_at<'a>(shape: &'a Shape, navs: &[Nav]) -> Option<&'a Shape> {
    let mut cur = shape;
    for nav in navs {
        match (cur, nav) {
            (Shape::Record { fields: fs }, Nav::Field(name)) => {
                cur = &fs.iter().find(|(nn, _)| nn == name)?.1;
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

fn cshape_at_one<'a>(shape: &'a Shape, nav: &Nav) -> Option<&'a Shape> {
    match (shape, nav) {
        (Shape::Record { fields: fs }, Nav::Field(name)) => {
            fs.iter().find(|(nn, _)| nn == name).map(|(_, s)| s)
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

fn cgen_put_record(r: &mut CodecRng, fs: &[(String, Shape)]) -> Value {
    let mut pairs = Vec::new();
    for (name, _) in fs {
        if r.next(2) == 0 {
            pairs.push((t(name), cgen_scalar_leaf(r)));
        }
    }
    Value::Map(pairs)
}

/// Plant a collecting nav at the first collection node of a nav run
/// (independent-side reads).
fn cplant_collector(r: &mut CodecRng, schema: &Shape, rnavs: &mut Vec<Nav>) {
    let mut cut = None;
    let mut cur = schema;
    for (ii, nav) in rnavs.iter().enumerate() {
        match (cur, nav) {
            (Shape::Map { .. } | Shape::List { .. } | Shape::Deque { .. }, _) => {
                cut = Some(ii);
                break;
            }
            (s, nn) => match cshape_at_one(s, nn) {
                Some(next) => cur = next,
                None => break,
            },
        }
    }
    if let Some(ii) = cut {
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
        rnavs.truncate(ii + 1);
        rnavs.push(coll);
    }
}

fn cread_eval(db: &Db, schema: &Shape, qq: &Query, collecting: bool) -> Option<String> {
    let v = if collecting {
        query::select(db, schema, qq).ok().map(|vs| format!("{vs:?}"))?
    } else {
        match qq.navs.len() % 3 {
            0 => query::subtree(db, schema, qq).ok().map(|vv| format!("{vv:?}"))?,
            1 => query::one(db, schema, qq).ok().map(|vv| format!("{vv:?}"))?,
            _ => query::has(db, schema, qq).ok().map(|vv| format!("{vv:?}"))?,
        }
    };
    Some(v)
}

fn crun_fuzz(seed: u64) -> (u64, u64, u64) {
    let (_d, db) = open();
    let schemas = cshapes();
    let mut changed_n = 0u64;
    let mut overlap_n = 0u64;
    let mut iters = 0u64;

    for round in 0..300 {
        let mut r = CodecRng(seed.wrapping_add((round as u64).wrapping_mul(0x9E3779B97F4A7C15)));
        let schema = &schemas[r.next(schemas.len() as u64) as usize].1;

        // ---- random write ----
        let mut wsteps = Vec::new();
        cwalk(&mut r, schema, &mut wsteps, true);
        let wnavs = match navs_for(schema, &wsteps) {
            Ok(nn) => nn,
            Err(_) => continue,
        };
        let target = match cshape_at(schema, &wnavs) {
            Some(s) => s,
            None => continue,
        };
        let writes: Vec<Write> = match r.next(5) {
            0 => match target {
                Shape::Leaf => match dynpath::put(schema, None, &wnavs, &cgen_scalar(&mut r)) {
                    Ok(w) => w,
                    Err(_) => continue,
                },
                Shape::Sum => match dynpath::put(schema, None, &wnavs, &cn(r.next(10) as i64)) {
                    Ok(w) => w,
                    Err(_) => continue,
                },
                Shape::Record { .. } => {
                    let Shape::Record { fields: fs } = target else { continue };
                    match dynpath::put(schema, None, &wnavs, &cgen_put_record(&mut r, fs)) {
                        Ok(w) => w,
                        Err(_) => continue,
                    }
                }
                _ => continue,
            },
            1 => match target {
                Shape::Sum => match dynpath::add(schema, None, &wnavs, &cn(1 + r.next(5) as i64)) {
                    Ok(w) => vec![w],
                    Err(_) => continue,
                },
                _ => continue,
            },
            2 => match dynpath::push(schema, None, &wnavs, &cgen_scalar_leaf(&mut r)) {
                Ok(w) => vec![w],
                Err(_) => continue,
            },
            3 => match dynpath::delete(schema, None, &wnavs) {
                Ok(w) => vec![w],
                Err(_) => continue,
            },
            _ => match dynpath::clear(schema, None, &wnavs) {
                Ok(w) => vec![w],
                Err(_) => continue,
            },
        };
        if writes.is_empty() {
            continue;
        }

        // ---- random read: half the time, read AT the write so the pair
        // interacts; otherwise wander independently for the negative side.
        let rnavs: Vec<Nav> = if r.next(2) == 0 && !wnavs.is_empty() {
            let cut = r.next(wnavs.len() as u64 + 1) as usize;
            let mut v: Vec<Nav> = wnavs[..cut.min(wnavs.len())].to_vec();
            if r.next(3) == 0 {
                if let Some(s) = cshape_at(schema, &v) {
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
            cwalk(&mut r, schema, &mut rsteps, false);
            let mut v = match navs_for(schema, &rsteps) {
                Ok(nn) => nn,
                Err(_) => continue,
            };
            if r.next(3) == 0 {
                cplant_collector(&mut r, schema, &mut v);
            }
            v
        };
        let qq = Query::new(rnavs.clone());
        let collecting = qq.is_collecting();
        let before = match cread_eval(&db, schema, &qq, collecting) {
            Some(v) => v,
            None => continue,
        };

        if db.apply(&writes, Durability::DisableWal).is_err() {
            continue;
        }
        let after = match cread_eval(&db, schema, &qq, collecting) {
            Some(v) => v,
            None => continue,
        };
        iters += 1;

        let changed = before != after;
        let hit = flag_bidir(&Range::prefix(&read_prefix(&schema, &rnavs)), &writes);
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
    }
    (iters, changed_n, overlap_n)
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 64, .. ProptestConfig::default() })]
    #[test]
    fn codec_fuzz_changed_implies_overlap(seed in any::<u64>()) {
        let (iters, changed, overlapped) = crun_fuzz(seed);
        // Every seed must exercise real changes, or the fuzz is vacuous.
        prop_assert!(iters > 40, "too few iterations: {iters}");
        prop_assert!(changed > 0, "no read ever changed — fuzz writes nothing?");
        // …and overlap must fire (else the courier never repaints).
        prop_assert!(overlapped > 0, "overlap never fired — read sets broken?");
    }
}
