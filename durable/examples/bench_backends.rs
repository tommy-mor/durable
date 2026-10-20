//! Cross-backend benchmark: on-disk structures vs their in-memory BigO.
//!
//! The question this answers: does putting a Vec/VecDeque/BTreeMap on disk
//! change its complexity? Every chart plots rocksdb / redb / sled against
//! the std baseline (`memory`) on log–log axes with O(1)/O(N) slope guides.
//!
//! Run once per backend (sequentially — never in parallel, they contend),
//! then render the combined report:
//!
//! ```bash
//! OUT=target/bench-backends
//! cargo run -q -p durable --example bench_backends --no-default-features --features backend-rocksdb -- --work $OUT
//! cargo run -q -p durable --example bench_backends --no-default-features --features backend-redb -- --work $OUT
//! cargo run -q -p durable --example bench_backends --no-default-features --features backend-sled -- --work $OUT
//! # --render builds the harness with any backend (redb is fastest to compile):
//! cargo run -q -p durable --example bench_backends --no-default-features --features backend-redb -- --render $OUT
//! # open $OUT/report.html
//! ```
//!
//! `--quick` runs a small-N smoke pass. Methodology follows
//! `docs/bench-bigO.txt`: separate phases with stated durability, cold
//! reopen before read phases, medians over repeated samples. Caveat from
//! that doc we knowingly bend: datasets here top out at 1e6 rows (not
//! >>RAM), so hot-cache effects flatter scan curves — the slope guides
//! still show the shape, constants are machine-specific.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::time::Instant;

use durable::{Db, Durability, Durable, Deque, Leaf, List, Map};
use serde::{Deserialize, Serialize};

#[cfg(feature = "backend-rocksdb")]
const BACKEND: &str = "rocksdb";
#[cfg(feature = "backend-redb")]
const BACKEND: &str = "redb";
#[cfg(feature = "backend-sled")]
const BACKEND: &str = "sled";

const NS: [usize; 4] = [1_000, 10_000, 100_000, 1_000_000];
const QUICK_NS: [usize; 2] = [1_000, 10_000];
const BATCH_ROWS: usize = 10_000;
const SINGLE_REPS: usize = 100;
const GET_SAMPLES: usize = 2_000;
const POP_REPS: usize = 1_000;
const DEQUE_POP_REPS: usize = 500;

/// Keep in sync with Cargo.toml/Cargo.lock on every backend bump.
#[cfg(feature = "backend-rocksdb")]
const BACKEND_VERSION: &str = "rocksdb 0.25.0 (RocksDB 11.8.1)";
#[cfg(feature = "backend-redb")]
const BACKEND_VERSION: &str = "redb 4.4.0";
#[cfg(feature = "backend-sled")]
const BACKEND_VERSION: &str = "sled 0.34.7";

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
struct Rec {
    id: u64,
    name: String,
    active: bool,
}

#[derive(Durable)]
#[allow(dead_code)]
struct Bench {
    list: List<Leaf<u64>>,
    deque: Deque<Leaf<u64>>,
    maps: Map<String, Leaf<u64>>,
    nested: Map<String, Map<String, Map<String, Leaf<u64>>>>,
    records: List<Leaf<Rec>>,
}

// ---------------------------------------------------------------------------
// samples + stats
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct Sample {
    op: String,
    n: usize,
    p50_ns: f64,
    p99_ns: f64,
    unit: String,
}

fn stats(op: &str, n: usize, unit: &str, mut xs: Vec<u128>) -> Sample {
    xs.sort_unstable();
    let q = |p: f64| xs[((xs.len() as f64 * p).floor() as usize).min(xs.len() - 1)] as f64;
    Sample {
        op: op.to_string(),
        n,
        p50_ns: q(0.5),
        p99_ns: q(0.99),
        unit: unit.to_string(),
    }
}

fn total(op: &str, n: usize, unit: &str, ns: u128) -> Sample {
    Sample {
        op: op.to_string(),
        n,
        p50_ns: ns as f64,
        p99_ns: 0.0,
        unit: unit.to_string(),
    }
}

fn per_row(op: &str, n: usize, ns: u128, rows: usize) -> Sample {
    Sample {
        op: op.to_string(),
        n,
        p50_ns: ns as f64 / rows as f64,
        p99_ns: 0.0,
        unit: "ns/op".into(),
    }
}

/// Deterministic xorshift64* — std-only "random" keys.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
}

fn key(i: usize) -> String {
    format!("k{i:08}")
}

fn rec(i: usize) -> Rec {
    Rec {
        id: i as u64,
        name: format!("name-{i:08}"),
        active: i % 2 == 0,
    }
}

// ---------------------------------------------------------------------------
// --work driver
// ---------------------------------------------------------------------------

fn apply_all(db: &Db, writes: Vec<durable::Write>, durability: Durability) -> durable::Result<()> {
    for chunk in writes.chunks(BATCH_ROWS) {
        db.apply(chunk, durability)?;
    }
    Ok(())
}

fn work(out_dir: &Path, quick: bool) -> durable::Result<()> {
    let ns: &[usize] = if quick { &QUICK_NS } else { &NS };
    let base = tempfile::tempdir().map_err(|e| durable::Error::Log(e.to_string()))?;
    let mut samples = Vec::new();

    for &n in ns {
        println!("[{BACKEND}] n={n}");
        let path = base.path().join(format!("db-{n}"));
        wipe(&path)?;
        let db = Db::open(&path)?;
        let root = Bench::root();

        // ---- bulk loads (DisableWal, batched): the per-op floors ----
        let t = Instant::now();
        let mut batch = db.batch();
        for i in 0..n {
            batch.push(&root.list(), &(i as u64))?;
            if i % BATCH_ROWS == BATCH_ROWS - 1 {
                let fresh = db.batch();
                std::mem::replace(&mut batch, fresh)
                    .commit_with(Durability::DisableWal)?;
            }
        }
        batch.commit_with(Durability::DisableWal)?;
        samples.push(per_row("list_push_batch", n, t.elapsed().as_nanos(), n));

        let t = Instant::now();
        let mut batch = db.batch();
        for i in 0..n {
            batch.write(root.deque().push_back_op(&(i as u64)));
            batch.write(root.deque().push_front_op(&(i as u64)));
            if i % (BATCH_ROWS / 2) == (BATCH_ROWS / 2) - 1 {
                let fresh = db.batch();
                std::mem::replace(&mut batch, fresh)
                    .commit_with(Durability::DisableWal)?;
            }
        }
        batch.commit_with(Durability::DisableWal)?;
        samples.push(per_row("deque_push_batch", n, t.elapsed().as_nanos(), 2 * n));

        let t = Instant::now();
        let writes: Vec<_> = (0..n)
            .map(|i| root.maps().key(&key(i)).set(&(i as u64)))
            .collect();
        apply_all(&db, writes, Durability::DisableWal)?;
        samples.push(per_row(
            "map_put_batch",
            n,
            t.elapsed().as_nanos(),
            n,
        ));

        let t = Instant::now();
        let mut batch = db.batch();
        for i in 0..n {
            batch.push(&root.records(), &rec(i))?;
            if i % BATCH_ROWS == BATCH_ROWS - 1 {
                let fresh = db.batch();
                std::mem::replace(&mut batch, fresh)
                    .commit_with(Durability::DisableWal)?;
            }
        }
        batch.commit_with(Durability::DisableWal)?;
        samples.push(per_row("record_put_batch", n, t.elapsed().as_nanos(), n));

        // nested maps: fanout f with f^3 leaves ~= n
        let f = (n as f64).cbrt().ceil() as usize;
        let leaves = f * f * f;
        let t = Instant::now();
        let mut writes = Vec::with_capacity(BATCH_ROWS);
        for a in 0..f {
            for b in 0..f {
                for c in 0..f {
                    writes.push(
                        root.nested()
                            .key(&format!("a{a:04}"))
                            .key(&format!("b{b:04}"))
                            .key(&format!("c{c:04}"))
                            .set(&((a * f * f + b * f + c) as u64)),
                    );
                    if writes.len() == BATCH_ROWS {
                        db.apply(
                            &std::mem::replace(&mut writes, Vec::with_capacity(BATCH_ROWS)),
                            Durability::DisableWal,
                        )?;
                    }
                }
            }
        }
        if !writes.is_empty() {
            db.apply(&writes, Durability::DisableWal)?;
        }
        samples.push(per_row(
            "nested_put_batch",
            leaves,
            t.elapsed().as_nanos(),
            leaves,
        ));

        // ---- single-commit fsync floor (SyncWal List::push) ----
        let mut lat = Vec::with_capacity(SINGLE_REPS);
        for i in 0..SINGLE_REPS {
            let t = Instant::now();
            root.list().push(&db, &(1_000_000_000 + i as u64))?;
            lat.push(t.elapsed().as_nanos());
        }
        // (pop the probes back off so later phases see exactly n list rows)
        for _ in 0..SINGLE_REPS {
            root.list().pop(&db)?;
        }
        samples.push(stats("list_push_single", n, "ns/commit", lat));

        // ---- cold reopen, then all read phases ----
        drop(db);
        let db = Db::open(&path)?;

        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        let mut lat = Vec::with_capacity(GET_SAMPLES);
        for _ in 0..GET_SAMPLES {
            let i = (rng.next() % n as u64) as u64;
            let t = Instant::now();
            let v = root.list().get(&db, i)?;
            std::hint::black_box(v);
            lat.push(t.elapsed().as_nanos());
        }
        samples.push(stats("list_get", n, "ns", lat));

        let mut lat = Vec::with_capacity(GET_SAMPLES);
        for _ in 0..GET_SAMPLES {
            let k = key((rng.next() % n as u64) as usize);
            let t = Instant::now();
            let v = root.maps().get(&db, &k)?;
            std::hint::black_box(v);
            lat.push(t.elapsed().as_nanos());
        }
        samples.push(stats("map_get", n, "ns", lat));

        let mut lat = Vec::with_capacity(1000);
        for _ in 0..1000 {
            let (a, b, c) = (
                (rng.next() % f as u64) as usize,
                (rng.next() % f as u64) as usize,
                (rng.next() % f as u64) as usize,
            );
            let t = Instant::now();
            let v = root
                .nested()
                .key(&format!("a{a:04}"))
                .key(&format!("b{b:04}"))
                .key(&format!("c{c:04}"))
                .get(&db)?;
            std::hint::black_box(v);
            lat.push(t.elapsed().as_nanos());
        }
        samples.push(stats("nested_get_deep", leaves, "ns", lat));

        let t = Instant::now();
        let items = root.list().iter(&db)?;
        assert_eq!(items.len(), n);
        samples.push(total("list_iter", n, "ns total", t.elapsed().as_nanos()));

        let t = Instant::now();
        let dq = root.deque().iter(&db)?;
        assert_eq!(dq.len(), 2 * n);
        samples.push(total("deque_iter", n, "ns total", t.elapsed().as_nanos()));

        let t = Instant::now();
        let pairs = root.maps().iter(&db)?;
        assert_eq!(pairs.len(), n);
        samples.push(total("map_iter", n, "ns total", t.elapsed().as_nanos()));

        let t = Instant::now();
        let keys = root.maps().keys(&db)?;
        assert_eq!(keys.len(), n);
        samples.push(total("map_keys", n, "ns total", t.elapsed().as_nanos()));

        let t = Instant::now();
        let l1 = root.nested().keys(&db)?;
        assert_eq!(l1.len(), f);
        samples.push(total("nested_keys_l1", leaves, "ns total", t.elapsed().as_nanos()));

        let t = Instant::now();
        let mut deep = 0usize;
        for (_, pa) in root.nested().entries(&db)? {
            for (_, pb) in pa.entries(&db)? {
                deep += pb.iter(&db)?.len();
            }
        }
        assert_eq!(deep, leaves);
        samples.push(total(
            "nested_scan_deep",
            leaves,
            "ns total",
            t.elapsed().as_nanos(),
        ));

        let t = Instant::now();
        let recs = root.records().iter(&db)?;
        assert_eq!(recs.len(), n);
        samples.push(total(
            "record_scan",
            n,
            "ns total",
            t.elapsed().as_nanos(),
        ));

        // ---- destructive tail: pops + clear (nothing after but rebuild) ----
        let mut lat = Vec::with_capacity(POP_REPS);
        for _ in 0..POP_REPS {
            let t = Instant::now();
            let v = root.list().pop(&db)?;
            std::hint::black_box(v);
            lat.push(t.elapsed().as_nanos());
        }
        samples.push(stats("list_pop", n, "ns", lat));

        let mut lat = Vec::with_capacity(DEQUE_POP_REPS);
        for _ in 0..DEQUE_POP_REPS {
            let t = Instant::now();
            let v = root.deque().pop_front(&db)?;
            std::hint::black_box(v);
            lat.push(t.elapsed().as_nanos());
        }
        samples.push(stats("deque_pop_front", n, "ns", lat));
        let mut lat = Vec::with_capacity(DEQUE_POP_REPS);
        for _ in 0..DEQUE_POP_REPS {
            let t = Instant::now();
            let v = root.deque().pop_back(&db)?;
            std::hint::black_box(v);
            lat.push(t.elapsed().as_nanos());
        }
        samples.push(stats("deque_pop_back", n, "ns", lat));

        let t = Instant::now();
        db.apply(&[root.maps().clear()], Durability::SyncWal)?;
        samples.push(total("map_clear", n, "ns total", t.elapsed().as_nanos()));

        // ---- rebuild: wipe + replay everything from scratch ----
        drop(db);
        let rpath = base.path().join(format!("rebuild-{n}"));
        wipe(&rpath)?;
        let rdb = Db::open(&rpath)?;
        let rroot = Bench::root();
        let t = Instant::now();
        let mut batch = rdb.batch();
        for i in 0..n {
            batch.push(&rroot.list(), &(i as u64))?;
            if i % BATCH_ROWS == BATCH_ROWS - 1 {
                let fresh = rdb.batch();
                std::mem::replace(&mut batch, fresh)
                    .commit_with(Durability::DisableWal)?;
            }
        }
        batch.commit_with(Durability::DisableWal)?;
        let writes: Vec<_> = (0..n)
            .map(|i| rroot.maps().key(&key(i)).set(&(i as u64)))
            .collect();
        apply_all(&rdb, writes, Durability::DisableWal)?;
        samples.push(total("rebuild", n, "ns total", t.elapsed().as_nanos()));

        // ---- in-memory baselines (same op names, backend "memory") ----
        memory_baseline(n, leaves, f, &mut samples);
    }

    std::fs::create_dir_all(out_dir).map_err(|e| durable::Error::Log(e.to_string()))?;
    // tag memory samples distinctly (emitted by memory_baseline with backend field)
    let doc = serde_json::json!({
        "backend": BACKEND,
        "backend_version": BACKEND_VERSION,
        "samples": samples.iter().filter(|s| !s.op.starts_with("mem:")).map(|s| serde_json::json!({
            "op": s.op, "n": s.n, "p50_ns": s.p50_ns, "p99_ns": s.p99_ns, "unit": s.unit,
        })).collect::<Vec<_>>(),
    });
    let text = serde_json::to_string_pretty(&doc).map_err(|e| durable::Error::Log(e.to_string()))?;
    std::fs::write(out_dir.join(format!("results-{BACKEND}.json")), text)
        .map_err(|e| durable::Error::Log(e.to_string()))?;
    // memory baselines merge into their own file (identical across backends)
    let mem = serde_json::json!({
        "backend": "memory",
        "backend_version": "std Vec/VecDeque/BTreeMap",
        "samples": samples.iter().filter(|s| s.op.starts_with("mem:")).map(|s| serde_json::json!({
            "op": s.op.trim_start_matches("mem:"), "n": s.n, "p50_ns": s.p50_ns, "p99_ns": s.p99_ns, "unit": s.unit,
        })).collect::<Vec<_>>(),
    });
    let text = serde_json::to_string_pretty(&mem).map_err(|e| durable::Error::Log(e.to_string()))?;
    std::fs::write(out_dir.join("results-memory.json"), text)
        .map_err(|e| durable::Error::Log(e.to_string()))?;
    println!(
        "[{BACKEND}] wrote results-{BACKEND}.json + results-memory.json"
    );
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn memory_baseline(n: usize, leaves: usize, f: usize, out: &mut Vec<Sample>) {
    let mk = |op: &str, n: usize, unit: &str, xs: Vec<u128>| {
        let mut s = stats(op, n, unit, xs);
        s.op = format!("mem:{op}");
        s
    };
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);

    // Vec (List baseline)
    let t = Instant::now();
    let mut v = Vec::with_capacity(n);
    for i in 0..n as u64 {
        v.push(i);
    }
    out.push({
        let mut s = total("mem:list_push_batch", n, "ns/op", t.elapsed().as_nanos());
        s.p50_ns /= n as f64;
        s
    });
    let mut lat = Vec::with_capacity(GET_SAMPLES);
    for _ in 0..GET_SAMPLES {
        let i = (rng.next() % n as u64) as usize;
        let t = Instant::now();
        std::hint::black_box(v[i]);
        lat.push(t.elapsed().as_nanos());
    }
    out.push(mk("list_get", n, "ns", lat));
    let t = Instant::now();
    let mut acc = 0u64;
    for x in &v {
        acc = acc.wrapping_add(*x);
    }
    std::hint::black_box(acc);
    out.push(total("mem:list_iter", n, "ns total", t.elapsed().as_nanos()));
    let mut lat = Vec::with_capacity(POP_REPS);
    for _ in 0..POP_REPS {
        let t = Instant::now();
        std::hint::black_box(v.pop());
        lat.push(t.elapsed().as_nanos());
    }
    out.push(mk("list_pop", n, "ns", lat));

    // VecDeque (Deque baseline)
    let t = Instant::now();
    let mut vd: VecDeque<u64> = VecDeque::with_capacity(2 * n);
    for i in 0..n as u64 {
        vd.push_back(i);
        vd.push_front(i);
    }
    out.push({
        let mut s = total("mem:deque_push_batch", n, "ns/op", t.elapsed().as_nanos());
        s.p50_ns /= (2 * n) as f64;
        s
    });
    let t = Instant::now();
    let mut acc = 0u64;
    for x in &vd {
        acc = acc.wrapping_add(*x);
    }
    std::hint::black_box(acc);
    out.push(total("mem:deque_iter", n, "ns total", t.elapsed().as_nanos()));
    let mut lat = Vec::with_capacity(DEQUE_POP_REPS);
    for _ in 0..DEQUE_POP_REPS {
        let t = Instant::now();
        std::hint::black_box(vd.pop_front());
        lat.push(t.elapsed().as_nanos());
    }
    out.push(mk("deque_pop_front", n, "ns", lat));
    let mut lat = Vec::with_capacity(DEQUE_POP_REPS);
    for _ in 0..DEQUE_POP_REPS {
        let t = Instant::now();
        std::hint::black_box(vd.pop_back());
        lat.push(t.elapsed().as_nanos());
    }
    out.push(mk("deque_pop_back", n, "ns", lat));

    // BTreeMap (Map baseline)
    let t = Instant::now();
    let mut m: BTreeMap<String, u64> = BTreeMap::new();
    for i in 0..n {
        m.insert(key(i), i as u64);
    }
    out.push({
        let mut s = total("mem:map_put_batch", n, "ns/op", t.elapsed().as_nanos());
        s.p50_ns /= n as f64;
        s
    });
    let mut lat = Vec::with_capacity(GET_SAMPLES);
    for _ in 0..GET_SAMPLES {
        let k = key((rng.next() % n as u64) as usize);
        let t = Instant::now();
        std::hint::black_box(m.get(&k));
        lat.push(t.elapsed().as_nanos());
    }
    out.push(mk("map_get", n, "ns", lat));
    let t = Instant::now();
    let mut acc = 0u64;
    for (_, val) in &m {
        acc = acc.wrapping_add(*val);
    }
    std::hint::black_box(acc);
    out.push(total("mem:map_iter", n, "ns total", t.elapsed().as_nanos()));
    let t = Instant::now();
    let ks: Vec<String> = m.keys().cloned().collect();
    std::hint::black_box(&ks);
    out.push(total("mem:map_keys", n, "ns total", t.elapsed().as_nanos()));
    let t = Instant::now();
    m.clear();
    out.push(total("mem:map_clear", n, "ns total", t.elapsed().as_nanos()));

    // nested BTreeMaps (3-deep baseline)
    let t = Instant::now();
    let mut nm: BTreeMap<String, BTreeMap<String, BTreeMap<String, u64>>> = BTreeMap::new();
    for a in 0..f {
        for b in 0..f {
            for c in 0..f {
                nm.entry(format!("a{a:04}"))
                    .or_default()
                    .entry(format!("b{b:04}"))
                    .or_default()
                    .insert(format!("c{c:04}"), (a * f * f + b * f + c) as u64);
            }
        }
    }
    out.push({
        let mut s = total("mem:nested_put_batch", leaves, "ns/op", t.elapsed().as_nanos());
        s.p50_ns /= leaves as f64;
        s
    });
    let mut lat = Vec::with_capacity(1000);
    for _ in 0..1000 {
        let (a, b, c) = (
            (rng.next() % f as u64) as usize,
            (rng.next() % f as u64) as usize,
            (rng.next() % f as u64) as usize,
        );
        let t = Instant::now();
        std::hint::black_box(
            nm.get(&format!("a{a:04}"))
                .and_then(|x| x.get(&format!("b{b:04}")))
                .and_then(|x| x.get(&format!("c{c:04}"))),
        );
        lat.push(t.elapsed().as_nanos());
    }
    out.push(mk("nested_get_deep", leaves, "ns", lat));
    let t = Instant::now();
    let l1: Vec<String> = nm.keys().cloned().collect();
    std::hint::black_box(&l1);
    out.push(total(
        "mem:nested_keys_l1",
        leaves,
        "ns total",
        t.elapsed().as_nanos(),
    ));
    let t = Instant::now();
    let mut deep = 0u64;
    for (_, l2) in &nm {
        for (_, l3) in l2 {
            // Visit every leaf like the durable walk decodes every value.
            for v in l3.values() {
                deep = deep.wrapping_add(*v);
            }
        }
    }
    std::hint::black_box(deep);
    out.push(total(
        "mem:nested_scan_deep",
        leaves,
        "ns total",
        t.elapsed().as_nanos(),
    ));

    // Vec<Rec> (record list baseline)
    let t = Instant::now();
    let vr: Vec<Rec> = (0..n).map(rec).collect();
    out.push({
        let mut s = total("mem:record_put_batch", n, "ns/op", t.elapsed().as_nanos());
        s.p50_ns /= n as f64;
        s
    });
    let t = Instant::now();
    let mut acc = 0u64;
    for r in &vr {
        acc = acc.wrapping_add(r.id + r.name.len() as u64 + r.active as u64);
    }
    std::hint::black_box(acc);
    out.push(total("mem:record_scan", n, "ns total", t.elapsed().as_nanos()));
}

fn wipe(path: &Path) -> durable::Result<()> {
    if path.exists() {
        std::fs::remove_dir_all(path).map_err(|e| durable::Error::Log(e.to_string()))?;
    }
    std::fs::create_dir_all(path).map_err(|e| durable::Error::Log(e.to_string()))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// --render: SVG log-log charts + HTML report
// ---------------------------------------------------------------------------

fn fmt_time(ns: f64) -> String {
    if ns < 1_000.0 {
        format!("{ns:.0} ns")
    } else if ns < 1_000_000.0 {
        format!("{:.1} µs", ns / 1_000.0)
    } else if ns < 1_000_000_000.0 {
        format!("{:.1} ms", ns / 1_000_000.0)
    } else {
        format!("{:.2} s", ns / 1_000_000_000.0)
    }
}

fn fmt_n(n: f64) -> String {
    if n >= 1_000_000.0 {
        format!("{}M", n / 1_000_000.0)
    } else if n >= 1_000.0 {
        format!("{}k", n / 1_000.0)
    } else {
        format!("{n:.0}")
    }
}

const COLORS: [(&str, &str); 4] = [
    ("rocksdb", "#f0883e"),
    ("redb", "#58a6ff"),
    ("sled", "#3fb950"),
    ("memory", "#d29922"),
];

const ORDER: [&str; 4] = ["rocksdb", "redb", "sled", "memory"];

fn color_of(backend: &str) -> &str {
    COLORS
        .iter()
        .find(|(b, _)| *b == backend)
        .map(|(_, c)| *c)
        .unwrap_or("#d2a8ff")
}

struct ChartDef {
    op: String,
    title: String,
    y_label: String,
    metric: String, // "p50_ns" or "p99_ns"
    guide: Option<String>, // None | "flat" | "linear"
}

fn chart_svg(
    def: &ChartDef,
    backends: &HashMap<String, Vec<Sample>>,
    order: &[String],
) -> String {
    const W: f64 = 780.0;
    const H: f64 = 480.0;
    const ML: f64 = 92.0;
    const MR: f64 = 150.0;
    const MT: f64 = 44.0;
    const MB: f64 = 56.0;

    let mut all_x = vec![];
    let mut all_y = vec![];
    let mut series: Vec<(&String, Vec<(f64, f64)>)> = vec![];
    for b in order {
        if let Some(ss) = backends.get(b) {
            let pts: Vec<(f64, f64)> = ss
                .iter()
                .filter(|s| s.op == def.op)
                .map(|s| {
                    (
                        s.n as f64,
                        if def.metric == "p99_ns" {
                            s.p99_ns
                        } else {
                            s.p50_ns
                        },
                    )
                })
                .collect();
            if !pts.is_empty() {
                all_x.extend(pts.iter().map(|p| p.0));
                all_y.extend(pts.iter().map(|p| p.1.max(1.0)));
                series.push((b, pts));
            }
        }
    }
    if series.is_empty() {
        return String::new();
    }
    let (mut x0, mut x1) = (all_x[0], all_x[0]);
    let (mut y0, mut y1) = (all_y[0], all_y[0]);
    for &x in &all_x {
        x0 = x0.min(x);
        x1 = x1.max(x);
    }
    for &y in &all_y {
        y0 = y0.min(y);
        y1 = y1.max(y);
    }
    x0 /= 1.6;
    x1 *= 1.6;
    y0 /= 2.2;
    y1 *= 2.2;
    if (y1 / y0) < 4.0 {
        let mid = (y0 * y1).sqrt();
        y0 = mid / 2.0;
        y1 = mid * 2.0;
    }
    let (lx0, lx1, ly0, ly1) = (x0.log10(), x1.log10(), y0.log10(), y1.log10());
    let px = |x: f64| ML + (x.log10() - lx0) / (lx1 - lx0) * (W - ML - MR);
    let py = |y: f64| MT + (1.0 - (y.max(1.0).log10() - ly0) / (ly1 - ly0)) * (H - MT - MB);

    let mut s = String::new();
    s.push_str(&format!(
        "<svg viewBox=\"0 0 {W} {H}\" xmlns=\"http://www.w3.org/2000/svg\" \
         style=\"background:#16181d;border-radius:12px;width:100%;height:auto;display:block\">"
    ));
    s.push_str(&format!(
        "<text x=\"{}\" y=\"24\" fill=\"#e6edf3\" font-size=\"17\" font-family=\"sans-serif\" \
         text-anchor=\"middle\">{}</text>",
        W / 2.0,
        def.title
    ));
    // gridlines + ticks: x at decades
    let mut p = lx0.ceil() as i32;
    while (p as f64) <= lx1 {
        let x = px(10f64.powi(p));
        s.push_str(&format!(
            "<line x1=\"{x}\" y1=\"{MT}\" x2=\"{x}\" y2=\"{}\" stroke=\"#2a2e37\"/>",
            H - MB
        ));
        s.push_str(&format!(
            "<text x=\"{x}\" y=\"{}\" fill=\"#8b949e\" font-size=\"12\" font-family=\"sans-serif\" \
             text-anchor=\"middle\">{}</text>",
            H - MB + 22.0,
            fmt_n(10f64.powi(p))
        ));
        p += 1;
    }
    // y ticks at decades with time labels
    let mut q = ly0.floor() as i32;
    while (q as f64) <= ly1 {
        let v = 10f64.powi(q);
        let y = py(v);
        s.push_str(&format!(
            "<line x1=\"{ML}\" y1=\"{y}\" x2=\"{}\" y2=\"{y}\" stroke=\"#2a2e37\"/>",
            W - MR
        ));
        s.push_str(&format!(
            "<text x=\"{}\" y=\"{}\" fill=\"#8b949e\" font-size=\"12\" font-family=\"sans-serif\" \
             text-anchor=\"end\">{}</text>",
            ML - 10.0,
            y + 4.0,
            fmt_time(v)
        ));
        q += 1;
    }
    // axes labels
    s.push_str(&format!(
        "<text x=\"{}\" y=\"{}\" fill=\"#8b949e\" font-size=\"12\" font-family=\"sans-serif\" \
         text-anchor=\"middle\">dataset size N (rows)</text>",
        (ML + W - MR) / 2.0,
        H - 8.0
    ));
    s.push_str(&format!(
        "<text x=\"16\" y=\"{}\" fill=\"#8b949e\" font-size=\"12\" font-family=\"sans-serif\" \
         text-anchor=\"middle\" transform=\"rotate(-90 16 {})\">{} ({})</text>",
        (MT + H - MB) / 2.0,
        (MT + H - MB) / 2.0,
        def.y_label,
        if def.metric == "p99_ns" {
            "p99"
        } else {
            "p50"
        }
    ));
    // guides
    if let Some(g) = &def.guide {
        let (ax, ay) = series[0].1[0];
        let (gx0, gx1) = (x0, x1);
        let (gy0, gy1) = match g.as_str() {
            "flat" => (ay, ay),
            _ => (ay, ay * (gx1 / ax)),
        };
        s.push_str(&format!(
            "<line x1=\"{}\" y1=\"{}\" x2=\"{}\" y2=\"{}\" stroke=\"#6e7681\" \
             stroke-dasharray=\"7 5\" stroke-width=\"1.5\"/>",
            px(gx0),
            py(gy0),
            px(gx1),
            py(gy1)
        ));
        let label = if g == "flat" { "O(1)" } else { "O(N)" };
        s.push_str(&format!(
            "<text x=\"{}\" y=\"{}\" fill=\"#6e7681\" font-size=\"12\" font-family=\"sans-serif\">\
             ╌ {label}</text>",
            px(gx1) - 52.0,
            py(gy1) - 8.0
        ));
    }
    // series
    for (b, pts) in &series {
        let c = color_of(b);
        let d = pts
            .iter()
            .map(|(x, y)| format!("{} {}", px(*x), py(*y)))
            .collect::<Vec<_>>()
            .join(" L");
        s.push_str(&format!(
            "<polyline points=\"{d}\" fill=\"none\" stroke=\"{c}\" stroke-width=\"2.5\"/>"
        ));
        for (x, y) in pts {
            s.push_str(&format!(
                "<circle cx=\"{}\" cy=\"{}\" r=\"4.5\" fill=\"{c}\" stroke=\"#16181d\" \
                 stroke-width=\"1.5\"/>",
                px(*x),
                py(*y)
            ));
        }
    }
    // legend
    for (i, (b, _)) in series.iter().enumerate() {
        let y = MT + 6.0 + i as f64 * 22.0;
        let c = color_of(b);
        s.push_str(&format!(
            "<rect x=\"{}\" y=\"{}\" width=\"14\" height=\"14\" rx=\"3\" fill=\"{c}\"/>",
            W - MR + 12.0,
            y - 11.0
        ));
        s.push_str(&format!(
            "<text x=\"{}\" y=\"{y}\" fill=\"#e6edf3\" font-size=\"13\" \
             font-family=\"sans-serif\">{b}</text>",
            W - MR + 32.0
        ));
    }
    s.push_str("</svg>");
    s
}

fn chart(op: &str, title: &str, y: &str, metric: &str, guide: Option<&str>) -> ChartDef {
    ChartDef {
        op: op.into(),
        title: title.into(),
        y_label: y.into(),
        metric: metric.into(),
        guide: guide.map(str::to_string),
    }
}

fn render(out_dir: &Path) -> durable::Result<()> {
    let err = |e: std::io::Error| durable::Error::Log(e.to_string());
    let mut backends: HashMap<String, Vec<Sample>> = HashMap::new();
    let mut versions: HashMap<String, String> = HashMap::new();
    let mut entries: Vec<PathBuf> = std::fs::read_dir(out_dir)
        .map_err(err)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with("results-") && n.ends_with(".json"))
                .unwrap_or(false)
        })
        .collect();
    entries.sort();
    for path in entries {
        let text = std::fs::read_to_string(&path).map_err(err)?;
        let doc: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| durable::Error::Log(e.to_string()))?;
        let backend = doc["backend"].as_str().unwrap_or("?").to_string();
        let mut ss = vec![];
        for v in doc["samples"].as_array().cloned().unwrap_or_default() {
            ss.push(Sample {
                op: v["op"].as_str().unwrap_or("").into(),
                n: v["n"].as_u64().unwrap_or(0) as usize,
                p50_ns: v["p50_ns"].as_f64().unwrap_or(0.0),
                p99_ns: v["p99_ns"].as_f64().unwrap_or(0.0),
                unit: v["unit"].as_str().unwrap_or("").into(),
            });
        }
        backends.insert(backend.clone(), ss);
        versions.insert(
            backend,
            doc["backend_version"].as_str().unwrap_or("?").to_string(),
        );
    }
    if backends.is_empty() {
        return Err(durable::Error::Log(format!(
            "no results-*.json in {}",
            out_dir.display()
        )));
    }
    let order: Vec<String> = ORDER
        .iter()
        .map(|s| s.to_string())
        .filter(|b| backends.contains_key(b))
        .collect();

    // (op, title, y-label, metric, guide)
    let charts = [
        // List vs Vec
        chart(
            "list_push_batch",
            "List push_back, batched: cost per push vs N",
            "ns / push",
            "p50_ns",
            Some("flat"),
        ),
        chart(
            "list_push_single",
            "List push, single SyncWal commit",
            "commit latency",
            "p50_ns",
            Some("flat"),
        ),
        chart(
            "list_get",
            "List get(i), cold",
            "get latency",
            "p50_ns",
            Some("flat"),
        ),
        chart(
            "list_iter",
            "List full iteration time vs N",
            "total time",
            "p50_ns",
            Some("linear"),
        ),
        chart(
            "list_pop",
            "List pop_back latency",
            "pop latency",
            "p50_ns",
            Some("flat"),
        ),
        // Deque vs VecDeque
        chart(
            "deque_push_batch",
            "Deque push (back+front), batched: cost per push vs N",
            "ns / push",
            "p50_ns",
            Some("flat"),
        ),
        chart(
            "deque_pop_front",
            "Deque pop_front latency",
            "pop latency",
            "p50_ns",
            Some("flat"),
        ),
        chart(
            "deque_pop_back",
            "Deque pop_back latency",
            "pop latency",
            "p50_ns",
            Some("flat"),
        ),
        chart(
            "deque_iter",
            "Deque full iteration time vs N",
            "total time",
            "p50_ns",
            Some("linear"),
        ),
        // Map vs BTreeMap
        chart(
            "map_put_batch",
            "Map put, batched: cost per put vs N",
            "ns / put",
            "p50_ns",
            Some("flat"),
        ),
        chart(
            "map_get",
            "Map get, cold",
            "get latency",
            "p50_ns",
            Some("flat"),
        ),
        chart(
            "map_iter",
            "Map full iteration time vs N",
            "total time",
            "p50_ns",
            Some("linear"),
        ),
        chart(
            "map_keys",
            "Map keys() scan time vs N",
            "total time",
            "p50_ns",
            Some("linear"),
        ),
        chart(
            "map_clear",
            "Map clear() (whole-subtree delete) vs N",
            "total time",
            "p50_ns",
            None,
        ),
        // Nested maps vs nested BTreeMaps
        chart(
            "nested_put_batch",
            "3-deep map put, batched: cost per leaf vs leaves",
            "ns / leaf",
            "p50_ns",
            Some("flat"),
        ),
        chart(
            "nested_get_deep",
            "3-deep map get at depth, cold",
            "get latency",
            "p50_ns",
            Some("flat"),
        ),
        chart(
            "nested_keys_l1",
            "3-deep map level-1 keys() vs leaves",
            "total time",
            "p50_ns",
            Some("linear"),
        ),
        chart(
            "nested_scan_deep",
            "3-deep map full recursive scan vs leaves",
            "total time",
            "p50_ns",
            Some("linear"),
        ),
        // Records
        chart(
            "record_scan",
            "List<Record> full scan vs N (O(N·F) vs O(N))",
            "total time",
            "p50_ns",
            Some("linear"),
        ),
        // Rebuild
        chart(
            "rebuild",
            "Projection rebuild (wipe + replay) vs N",
            "total time",
            "p50_ns",
            Some("linear"),
        ),
    ];

    let mut html = String::new();
    html.push_str(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\">\
         <title>durable backend benchmarks</title></head>\
         <body style=\"background:#0d1117;color:#e6edf3;font-family:sans-serif;\
         max-width:900px;margin:0 auto;padding:24px\">\
         <h1>durable structures on disk vs in memory</h1>",
    );
    html.push_str(
        "<p style=\"color:#8b949e\">Same workloads, same machine, sequential runs. \
         Log–log axes: O(1) is flat, O(N) follows the dashed slope-1 guide. \
         <b>memory</b> is std Vec/VecDeque/BTreeMap measured in-process. \
         Medians over repeated samples; read phases run after a cold reopen. \
         Datasets top out at 1M rows (below the &gt;&gt;RAM ideal in \
         docs/bench-bigO.txt — slopes, not constants, are the claim.</p>",
    );
    for def in &charts {
        let svg = chart_svg(def, &backends, &order);
        if svg.is_empty() {
            continue;
        }
        html.push_str(&format!("<h2>{}</h2>", def.title));
        html.push_str(&svg);
        // data table
        html.push_str(
            "<table style=\"border-collapse:collapse;margin:12px 0 32px;font-size:13px\">\
             <tr style=\"color:#8b949e\"><th style=\"text-align:left;padding:4px 12px\">N</th>",
        );
        for b in &order {
            html.push_str(&format!(
                "<th style=\"text-align:right;padding:4px 12px\" colspan=\"2\">{b} p50/p99</th>"
            ));
        }
        html.push_str("</tr>");
        let mut ns: Vec<usize> = backends
            .values()
            .flat_map(|ss| ss.iter().filter(|s| s.op == def.op).map(|s| s.n))
            .collect();
        ns.sort_unstable();
        ns.dedup();
        for n in ns {
            html.push_str(&format!(
                "<tr><td style=\"padding:4px 12px\">{}</td>",
                fmt_n(n as f64)
            ));
            for b in &order {
                let cell = backends
                    .get(b)
                    .and_then(|ss| ss.iter().find(|s| s.op == def.op && s.n == n))
                    .map(|s| {
                        let (a, b_) = if def.metric == "p99_ns" {
                            (s.p99_ns, s.p99_ns)
                        } else {
                            (s.p50_ns, s.p99_ns)
                        };
                        let p99 = if s.p99_ns > 0.0 {
                            fmt_time(b_)
                        } else {
                            "—".into()
                        };
                        format!(
                            "<td style=\"text-align:right;padding:4px 12px\">{}</td>\
                             <td style=\"text-align:right;padding:4px 12px;color:#8b949e\">{p99}</td>",
                            fmt_time(a)
                        )
                    })
                    .unwrap_or("<td></td><td></td>".into());
                html.push_str(&cell);
            }
            html.push_str("</tr>");
        }
        html.push_str("</table>");
    }
    html.push_str(
        "<h2>Notes</h2><ul style=\"color:#8b949e\">\
         <li>List/Deque/Map/Nested/Record reads and writes go through the same \
         key codec on every backend; only the storage engine differs.</li>\
         <li>SyncWal single commits pay the fsync floor; batched DisableWal \
         commits amortize it.</li>\
         <li>map_clear is a range tombstone on rocksdb, key expansion on \
         redb/sled — watch the shapes diverge.</li>",
    );
    for b in &order {
        let ver = versions.get(b).map(String::as_str).unwrap_or("?");
        html.push_str(&format!("<li>{b}: {ver}.</li>"));
    }
    html.push_str("</ul></body></html>");
    std::fs::write(out_dir.join("report.html"), html).map_err(err)?;
    println!("wrote {}", out_dir.join("report.html").display());
    Ok(())
}

// ---------------------------------------------------------------------------

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let out = args
        .iter()
        .position(|a| a == "--work" || a == "--render")
        .and_then(|i| args.get(i + 1))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target/bench-backends"));
    let res = if args.iter().any(|a| a == "--render") {
        render(&out)
    } else {
        work(&out, args.iter().any(|a| a == "--quick"))
    };
    if let Err(e) = res {
        eprintln!("bench_backends: {e}");
        std::process::exit(1);
    }
}
