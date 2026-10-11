//! Invalidation lab: is "read-prefix ∩ write-prefix" a sound way to decide
//! which views need a repaint after a commit?
//!
//! Ground truth: run the query before and after each write batch and compare
//! results. A strategy MISSES when the result changed but the strategy did
//! not flag it (a stale screen), and FALSE-POSITIVES when it flagged a view
//! whose result did not change (a wasted repaint).
//!
//!   cargo test -p durable --test invalidation_lab -- --nocapture

use ciborium::value::Integer;
use ciborium::Value;
use durable::{dynpath, query, Db, Describe, Deque, Durability, Durable, Expr, Leaf, List, Map, Nav, Op, Predicate, Query, Shape, Sum, Write};
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
