//! Cross-backend benchmark with BigO charts.
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

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use durable::{Db, Durability, Durable, Leaf, List, Map, Sum};

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
const READ_REPS: usize = 200;

/// Keep in sync with Cargo.toml/Cargo.lock on every backend bump.
#[cfg(feature = "backend-rocksdb")]
const BACKEND_VERSION: &str = "rocksdb 0.25.0 (RocksDB 11.8.1)";
#[cfg(feature = "backend-redb")]
const BACKEND_VERSION: &str = "redb 4.4.0";
#[cfg(feature = "backend-sled")]
const BACKEND_VERSION: &str = "sled 0.34.7";

#[derive(Durable)]
#[allow(dead_code)]
struct Bench {
    items: List<Leaf<u64>>,
    sums: Map<String, Sum<i64>>,
    maps: Map<String, Leaf<u64>>,
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

// ---------------------------------------------------------------------------
// workloads (each takes a fresh &Db)
// ---------------------------------------------------------------------------

fn load_maps(db: &Db, n: usize) -> durable::Result<()> {
    let root = Bench::root();
    let mut writes = Vec::with_capacity(BATCH_ROWS);
    for i in 0..n {
        writes.push(root.maps().key(&key(i)).set(&(i as u64)));
        if writes.len() == BATCH_ROWS {
            db.apply(&writes, Durability::DisableWal)?;
            writes.clear();
        }
    }
    if !writes.is_empty() {
        db.apply(&writes, Durability::DisableWal)?;
    }
    Ok(())
}

fn load_list(db: &Db, n: usize) -> durable::Result<()> {
    let items = Bench::root().items();
    let mut batch = db.batch();
    for i in 0..n {
        batch.push(&items, &(i as u64))?;
        if i % BATCH_ROWS == BATCH_ROWS - 1 {
            let fresh = db.batch();
            let pending = std::mem::replace(&mut batch, fresh);
            pending.commit_with(Durability::DisableWal)?;
        }
    }
    batch.commit_with(Durability::DisableWal)?;
    Ok(())
}

///latency of single-row SyncWal commits (the fsync floor).
fn bench_single_appends(db: &Db, reps: usize) -> durable::Result<Vec<u128>> {
    let items = Bench::root().items();
    let mut out = Vec::with_capacity(reps);
    for i in 0..reps {
        let mut batch = db.batch();
        batch.push(&items, &(1_000_000_000 + i as u64))?;
        let t = Instant::now();
        batch.commit_with(Durability::SyncWal)?;
        out.push(t.elapsed().as_nanos());
    }
    Ok(out)
}

/// Per-row cost of a 10k-row SyncWal group commit.
fn bench_batch_commit(db: &Db, rows: usize) -> durable::Result<u128> {
    let root = Bench::root();
    let writes: Vec<_> = (0..rows)
        .map(|i| root.maps().key(&format!("b{i:08}")).set(&(i as u64)))
        .collect();
    let t = Instant::now();
    db.apply(&writes, Durability::SyncWal)?;
    Ok(t.elapsed().as_nanos() / rows as u128)
}

fn bench_point_gets(db: &Db, n: usize, samples: usize) -> durable::Result<Vec<u128>> {
    let root = Bench::root();
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut out = Vec::with_capacity(samples);
    for _ in 0..samples {
        let k = key((rng.next() % n as u64) as usize);
        let t = Instant::now();
        let v = root.maps().get(db, &k)?;
        std::hint::black_box(v);
        out.push(t.elapsed().as_nanos());
    }
    Ok(out)
}

fn bench_scan(db: &Db) -> durable::Result<(u128, usize)> {
    let t = Instant::now();
    let pairs = Bench::root().maps().iter(db)?;
    let dt = t.elapsed().as_nanos();
    Ok((dt, pairs.len()))
}

/// M merges into ONE accumulator, then read it. Returns (total add ns, read samples).
fn bench_sum_pressure(
    db: &Db,
    m: usize,
) -> durable::Result<(u128, Vec<u128>)> {
    let acc = Bench::root().sums().key(&"pressure".to_string());
    let mut writes = Vec::with_capacity(BATCH_ROWS);
    let t = Instant::now();
    for _ in 0..m {
        writes.push(acc.add(1));
        if writes.len() == BATCH_ROWS {
            db.apply(&writes, Durability::SyncWal)?;
            writes.clear();
        }
    }
    if !writes.is_empty() {
        db.apply(&writes, Durability::SyncWal)?;
    }
    let add_total = t.elapsed().as_nanos();
    let mut reads = Vec::with_capacity(READ_REPS);
    for _ in 0..READ_REPS {
        let t = Instant::now();
        let v: i64 = acc.get(db)?;
        std::hint::black_box(v);
        reads.push(t.elapsed().as_nanos());
    }
    Ok((add_total, reads))
}

fn cold_reopen(path: &Path) -> durable::Result<Db> {
    Db::open(path)
}

fn wipe(path: &Path) -> durable::Result<()> {
    if path.exists() {
        std::fs::remove_dir_all(path).map_err(|e| durable::Error::Log(e.to_string()))?;
    }
    std::fs::create_dir_all(path).map_err(|e| durable::Error::Log(e.to_string()))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// --work driver
// ---------------------------------------------------------------------------

fn work(out_dir: &Path, quick: bool) -> durable::Result<()> {
    let ns: &[usize] = if quick { &QUICK_NS } else { &NS };
    let base = tempfile::tempdir().map_err(|e| durable::Error::Log(e.to_string()))?;
    let mut samples = Vec::new();

    for &n in ns {
        println!("[{BACKEND}] n={n}");
        let path = base.path().join(format!("db-{n}"));
        wipe(&path)?;
        let db = Db::open(&path)?;

        // bulk load (DisableWal batches) — throughput floor
        let t = Instant::now();
        load_maps(&db, n)?;
        load_list(&db, n)?;
        let load_ns = t.elapsed().as_nanos();
        samples.push(Sample {
            op: "load_bulk".into(),
            n,
            p50_ns: load_ns as f64 / (2 * n) as f64,
            p99_ns: 0.0,
            unit: "ns/row".into(),
        });

        // single-append fsync floor
        let single = bench_single_appends(&db, SINGLE_REPS.min(n.max(10)))?;
        samples.push(stats("append_single_syncwal", n, "ns/commit", single));

        // group commit
        let rows = BATCH_ROWS.min(n);
        let per_row = bench_batch_commit(&db, rows)?;
        samples.push(Sample {
            op: "batch_commit_syncwal".into(),
            n,
            p50_ns: per_row as f64,
            p99_ns: 0.0,
            unit: "ns/row".into(),
        });

        // cold point reads
        drop(db);
        let db = cold_reopen(&path)?;
        let gets = bench_point_gets(&db, n, GET_SAMPLES.min(n * 2))?;
        samples.push(stats("point_get_cold", n, "ns", gets));

        // full scan
        let (scan_ns, count) = bench_scan(&db)?;
        assert_eq!(count, n + BATCH_ROWS.min(n));
        samples.push(Sample {
            op: "scan_full".into(),
            n,
            p50_ns: scan_ns as f64,
            p99_ns: 0.0,
            unit: "ns total".into(),
        });

        // sum pressure on a fresh key space (separate dir: merges only)
        drop(db);
        let spath = base.path().join(format!("sum-{n}"));
        wipe(&spath)?;
        let sdb = Db::open(&spath)?;
        let (add_ns, reads) = bench_sum_pressure(&sdb, n)?;
        samples.push(Sample {
            op: "sum_add".into(),
            n,
            p50_ns: add_ns as f64 / n as f64,
            p99_ns: 0.0,
            unit: "ns/op".into(),
        });
        drop(sdb);
        let sdb = cold_reopen(&spath)?;
        let acc = Bench::root().sums().key(&"pressure".to_string());
        let v: i64 = acc.get(&sdb)?;
        assert_eq!(v, n as i64, "pressure accumulator mismatch");
        samples.push(stats("sum_read_pressure", n, "ns", reads));

        // rebuild: wipe + replay the same writes from scratch
        drop(sdb);
        let rpath = base.path().join(format!("rebuild-{n}"));
        wipe(&rpath)?;
        let rdb = Db::open(&rpath)?;
        let t = Instant::now();
        load_maps(&rdb, n)?;
        load_list(&rdb, n)?;
        let rebuild_ns = t.elapsed().as_nanos();
        samples.push(Sample {
            op: "rebuild".into(),
            n,
            p50_ns: rebuild_ns as f64,
            p99_ns: 0.0,
            unit: "ns total".into(),
        });
    }

    std::fs::create_dir_all(out_dir).map_err(|e| durable::Error::Log(e.to_string()))?;
    let doc = serde_json::json!({
        "backend": BACKEND,
        "backend_version": BACKEND_VERSION,
        "samples": samples.iter().map(|s| serde_json::json!({
            "op": s.op, "n": s.n, "p50_ns": s.p50_ns, "p99_ns": s.p99_ns, "unit": s.unit,
        })).collect::<Vec<_>>(),
    });
    let text = serde_json::to_string_pretty(&doc).map_err(|e| durable::Error::Log(e.to_string()))?;
    std::fs::write(out_dir.join(format!("results-{BACKEND}.json")), text)
        .map_err(|e| durable::Error::Log(e.to_string()))?;
    println!("[{BACKEND}] wrote results-{BACKEND}.json ({} samples)", samples.len());
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

const COLORS: [(&str, &str); 3] = [
    ("rocksdb", "#f0883e"),
    ("redb", "#58a6ff"),
    ("sled", "#3fb950"),
];

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

fn render(out_dir: &Path) -> durable::Result<()> {
    let err = |e: std::io::Error| durable::Error::Log(e.to_string());
    let mut backends: HashMap<String, Vec<Sample>> = HashMap::new();
    let mut versions: HashMap<String, String> = HashMap::new();
    let mut order: Vec<String> = vec![];
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
        order.push(backend.clone());
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

    let charts = [
        ChartDef {
            op: "append_single_syncwal".into(),
            title: "Single SyncWal append latency vs N".into(),
            y_label: "commit latency".into(),
            metric: "p50_ns".into(),
            guide: Some("flat".into()),
        },
        ChartDef {
            op: "batch_commit_syncwal".into(),
            title: "10k-row SyncWal group commit: cost per row vs N".into(),
            y_label: "ns / row".into(),
            metric: "p50_ns".into(),
            guide: Some("flat".into()),
        },
        ChartDef {
            op: "point_get_cold".into(),
            title: "Cold point-get latency vs N".into(),
            y_label: "get latency".into(),
            metric: "p50_ns".into(),
            guide: Some("flat".into()),
        },
        ChartDef {
            op: "point_get_cold".into(),
            title: "Cold point-get p99 vs N".into(),
            y_label: "get latency".into(),
            metric: "p99_ns".into(),
            guide: Some("flat".into()),
        },
        ChartDef {
            op: "scan_full".into(),
            title: "Full prefix scan time vs N".into(),
            y_label: "total scan time".into(),
            metric: "p50_ns".into(),
            guide: Some("linear".into()),
        },
        ChartDef {
            op: "sum_add".into(),
            title: "Sum accumulator add cost vs N merges".into(),
            y_label: "ns / add".into(),
            metric: "p50_ns".into(),
            guide: Some("flat".into()),
        },
        ChartDef {
            op: "sum_read_pressure".into(),
            title: "Sum read latency after N blind merges".into(),
            y_label: "read latency".into(),
            metric: "p50_ns".into(),
            guide: None,
        },
        ChartDef {
            op: "rebuild".into(),
            title: "Projection rebuild (wipe + replay) vs N".into(),
            y_label: "total rebuild time".into(),
            metric: "p50_ns".into(),
            guide: Some("linear".into()),
        },
    ];

    let mut html = String::new();
    html.push_str(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\">\
         <title>durable backend benchmarks</title></head>\
         <body style=\"background:#0d1117;color:#e6edf3;font-family:sans-serif;\
         max-width:900px;margin:0 auto;padding:24px\">\
         <h1>durable storage backends — BigO comparison</h1>",
    );
    html.push_str(
        "<p style=\"color:#8b949e\">Same workloads, same machine, sequential runs. \
         Log–log axes: O(1) is flat, O(N) follows the dashed slope-1 guide. \
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
                        format!(
                            "<td style=\"text-align:right;padding:4px 12px\">{}</td>\
                             <td style=\"text-align:right;padding:4px 12px;color:#8b949e\">{}</td>",
                            fmt_time(if def.metric == "p99_ns" {
                                s.p99_ns
                            } else {
                                s.p50_ns
                            }),
                            fmt_time(s.p99_ns)
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
         <li>rocksdb: blind Sum merges fold at compaction; reads slow under merge pressure.</li>\
         <li>redb: merges fold read-modify-write in the commit txn; reads stay flat.</li>\
         <li>sled: merges lower to put/delete against a commit overlay; reads stay flat.</li>\
         <li>SyncWal fsyncs per commit on rocksdb/redb; sled flush()es.</li>",
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
