//! rank — Rank Centrality (Negahban, Oh, Shah 2012, §3.1) as a pure native.
//!
//! `rank.scores(n, edges)` turns a deluge of weighted pairwise comparisons
//! into one ranking: the "general primitive" that reduces a stream of
//! `[winner, loser, weight]` edges down into a stationary distribution of
//! preference. Ported from slug's `server/src/ranking.rs` so the same edge
//! stream that feeds slug ranks here, oracle-checkable against `rank-eric.py`.
//!
//! The chain is the paper's construction: `a_ij` is the pairwise-normalized
//! probability `j` beats `i`; every off-diagonal divides by `d_max` (the
//! maximum *unweighted* degree) and the diagonal absorbs the remainder. The
//! degree divisor is what keeps a pure-star vote from oscillating — a
//! periodic chain — see slug issue #146; it is load-bearing, not cosmetic.

use std::collections::{BTreeMap, BTreeSet};

use crate::modules::{Module, NativeDef, NativeKind};
use crate::value::Value;

pub fn rank_module() -> Module {
    Module {
        name: "rank",
        natives: vec![NativeDef {
            name: "rank.scores",
            kind: NativeKind::Pure(rank_scores),
        }],
    }
}

struct Solution {
    pi: Vec<f64>,
    converged: bool,
    iterations: usize,
    residual: f64,
}

fn as_usize(v: &Value, what: &str) -> Result<usize, String> {
    match v {
        Value::Int(i) if *i >= 0 => Ok(*i as usize),
        other => Err(format!(
            "rank.scores: {what} must be a non-negative int, got {}",
            other.kind()
        )),
    }
}

fn as_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Int(i) => Some(*i as f64),
        Value::Float(f) => Some(*f),
        _ => None,
    }
}

/// `[winner, loser, weight]` triples → `(winner, loser) → mass`. A malformed
/// triple is skipped (data, not error): rank centrality is fed by a flood,
/// and one bad edge must not sink the whole ranking.
fn parse_edges(edges: &Value, n: usize) -> BTreeMap<(usize, usize), f64> {
    let mut mass: BTreeMap<(usize, usize), f64> = BTreeMap::new();
    let Value::Array(arr) = edges else {
        return mass;
    };
    for triple in arr.borrow().iter() {
        let Value::Array(t) = triple else { continue };
        let t = t.borrow();
        if t.len() != 3 {
            continue;
        }
        let w = match &t[0] {
            Value::Int(i) if *i >= 0 => *i as usize,
            _ => continue,
        };
        let l = match &t[1] {
            Value::Int(i) if *i >= 0 => *i as usize,
            _ => continue,
        };
        let Some(weight) = as_f64(&t[2]) else { continue };
        if w >= n || l >= n || w == l || weight <= 0.0 {
            continue;
        }
        *mass.entry((w, l)).or_insert(0.0) += weight;
    }
    mass
}

/// The sparse Rank Centrality chain, then power iteration to its stationary
/// distribution. `rows[i]` holds `(j, p)` where `p` is the probability `j`
/// beats `i`; `d_max` is the largest unweighted degree.
fn solve(n: usize, mass: &BTreeMap<(usize, usize), f64>, max_iters: usize, tol: f64) -> Solution {
    if n == 0 {
        return Solution { pi: vec![], converged: true, iterations: 0, residual: 0.0 };
    }
    if n == 1 {
        return Solution { pi: vec![1.0], converged: true, iterations: 0, residual: 0.0 };
    }

    // Deterministic: unordered pairs sorted, then pairwise normalization, so
    // the caller's edge order never leaks into the low bits of a score.
    let mut pairs: BTreeSet<(usize, usize)> = BTreeSet::new();
    for &(a, b) in mass.keys() {
        if a != b {
            pairs.insert((a.min(b), a.max(b)));
        }
    }

    let mut rows: Vec<Vec<(usize, f64)>> = vec![Vec::new(); n];
    for (x, y) in pairs {
        if x >= n || y >= n {
            continue;
        }
        let axy = *mass.get(&(x, y)).unwrap_or(&0.0);
        let ayx = *mass.get(&(y, x)).unwrap_or(&0.0);
        let total = axy + ayx;
        if total <= 0.0 {
            continue;
        }
        rows[x].push((y, ayx / total)); // y beats x
        rows[y].push((x, axy / total)); // x beats y
    }

    let d_max = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    if d_max == 0 {
        // No comparisons: everyone tied at uniform.
        return Solution {
            pi: vec![1.0 / n as f64; n],
            converged: true,
            iterations: 0,
            residual: 0.0,
        };
    }

    let outsum: Vec<f64> = rows.iter().map(|r| r.iter().map(|&(_, a)| a).sum()).collect();

    // One sweep: next[j] = Σ_i pi[i] · P[i][j], sparse (O(E)), left eigenvector.
    let mut pi = vec![1.0 / n as f64; n];
    let mut next = vec![0.0; n];
    let mut iterations = 0;
    let mut residual = f64::INFINITY;
    while iterations < max_iters {
        for j in 0..n {
            next[j] = pi[j] * (1.0 - outsum[j] / d_max as f64);
        }
        for i in 0..n {
            let p = pi[i] / d_max as f64;
            for &(j, a) in &rows[i] {
                next[j] += p * a;
            }
        }
        let mut diff = 0.0;
        for j in 0..n {
            diff += (next[j] - pi[j]).abs();
        }
        std::mem::swap(&mut pi, &mut next);
        iterations += 1;
        residual = diff;
        if diff < tol {
            break;
        }
    }

    Solution { pi, converged: residual < tol, iterations, residual }
}

/// rank.scores(n, edges, max_iters?, tol?) →
///   { scores = [f64], order = [int], converged = bool, iterations = int, residual = float }
///
/// `edges` is an array of `[winner, loser, weight]` triples. `weight` is the
/// evidence mass that `winner` beats `loser` — a slug ratio numerator, or a
/// decision model's calibrated `P(winner)`. `scores[i]` is the stationary mass
/// of item `i`; `order` is the indices sorted by score descending (hop has no
/// sort native, so the ranker hands back the order itself). `converged`
/// reports whether the solver actually reached `tol`, never swallowed.
pub fn rank_scores(args: Vec<Value>) -> Result<Value, String> {
    let n = match args.first() {
        Some(v) => as_usize(v, "n")?,
        None => return Err("rank.scores(n, edges, ...) needs n".into()),
    };
    let edges = match args.get(1) {
        Some(v) => v,
        None => return Err("rank.scores(n, edges, ...) needs edges".into()),
    };
    let max_iters = match args.get(2) {
        Some(Value::Int(i)) if *i > 0 => *i as usize,
        Some(other) if !matches!(other, Value::Nil) => {
            return Err(format!(
                "rank.scores: max_iters must be a positive int, got {}",
                other.kind()
            ))
        }
        _ => 100_000,
    };
    let tol = match args.get(3) {
        Some(Value::Float(f)) if *f > 0.0 => *f,
        Some(Value::Int(i)) if *i > 0 => *i as f64,
        Some(other) if !matches!(other, Value::Nil) => {
            return Err(format!(
                "rank.scores: tol must be positive, got {}",
                other.kind()
            ))
        }
        _ => 1e-8,
    };

    let mass = parse_edges(edges, n);
    let sol = solve(n, &mass, max_iters, tol);

    let pi = sol.pi;
    let mut order: Vec<usize> = (0..pi.len()).collect();
    order.sort_by(|&a, &b| pi[b].partial_cmp(&pi[a]).unwrap_or(std::cmp::Ordering::Equal));
    let scores = pi.into_iter().map(Value::Float).collect::<Vec<_>>();
    let order_vals = order.into_iter().map(|i| Value::Int(i as i64)).collect::<Vec<_>>();

    let mut out = BTreeMap::new();
    out.insert(Value::str("scores"), Value::array(scores));
    out.insert(Value::str("order"), Value::array(order_vals));
    out.insert(Value::str("converged"), Value::Bool(sol.converged));
    out.insert(Value::str("iterations"), Value::Int(sol.iterations as i64));
    out.insert(Value::str("residual"), Value::Float(sol.residual));
    Ok(Value::map(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full(n: usize, edges: Vec<(usize, usize, f64)>) -> Value {
        let arr = Value::array(
            edges
                .into_iter()
                .map(|(w, l, wt)| {
                    Value::array(vec![
                        Value::Int(w as i64),
                        Value::Int(l as i64),
                        Value::Float(wt),
                    ])
                })
                .collect(),
        );
        rank_scores(vec![Value::Int(n as i64), arr]).expect("rank.scores")
    }

    fn scores(n: usize, edges: Vec<(usize, usize, f64)>) -> Vec<f64> {
        match full(n, edges).get_field("scores") {
            Value::Array(a) => a
                .borrow()
                .iter()
                .map(|x| match x {
                    Value::Float(f) => *f,
                    _ => panic!("non-float score"),
                })
                .collect(),
            _ => panic!("no scores in result"),
        }
    }

    fn order(n: usize, edges: Vec<(usize, usize, f64)>) -> Vec<usize> {
        match full(n, edges).get_field("order") {
            Value::Array(a) => a
                .borrow()
                .iter()
                .map(|x| match x {
                    Value::Int(i) => *i as usize,
                    _ => panic!("non-int order index"),
                })
                .collect(),
            _ => panic!("no order in result"),
        }
    }

    /// The rank-eric.py oracle. A spanning tree (chain) over n items whose
    /// latent values are 1..n: item i+1 beats item i in the exact ratio. Rank
    /// centrality must recover the total order with scores proportional to the
    /// latent value — `scaled = score · n(n+1)/2` lands on `i+1` for every item.
    #[test]
    fn spanning_tree_recovers_order() {
        let n = 26usize;
        let mut edges = Vec::new();
        for i in 0..n - 1 {
            edges.push((i + 1, i, (i as f64) + 2.0)); // i+1 beats i
            edges.push((i, i + 1, (i as f64) + 1.0)); // i beats i+1
        }
        let s = scores(n, edges.clone());
        let scale = (n * (n + 1) / 2) as f64;
        for i in 0..n {
            assert!(s[i].is_finite());
            let scaled = s[i] * scale;
            assert!(
                (scaled - (i as f64 + 1.0)).abs() < 1e-3,
                "item {i}: scaled {scaled}, want {}",
                i + 1
            );
        }
        // and the handed-back order matches scores, descending
        assert_eq!(order(n, edges), (0..n).rev().collect::<Vec<usize>>());
    }

    /// Issue #146: a pure forward star (zebra beats everyone 2:1) used to yield
    /// a periodic chain that oscillated and came back uniform. The degree
    /// divisor keeps the diagonal positive and the winner lands on top.
    #[test]
    fn star_topology_converges_winner_on_top() {
        let edges = vec![(0, 1, 2.0), (1, 0, 1.0), (0, 2, 2.0), (2, 0, 1.0)];
        let s = scores(3, edges);
        assert!(s[0] > s[1] && s[0] > s[2], "zebra should rank top: {s:?}");
    }

    /// Same edges, different order → bit-close scores. Accumulation runs in
    /// sorted key order, so caller iteration order must not leak in.
    #[test]
    fn edge_order_does_not_change_scores() {
        let n = 30usize;
        let mut a = Vec::new();
        for i in 0..n - 1 {
            a.push((i + 1, i, 2.0));
            a.push((i, i + 1, 1.0));
        }
        let rev: Vec<_> = a.iter().rev().copied().collect();
        let sa = scores(n, a);
        let sb = scores(n, rev);
        for i in 0..n {
            assert!(
                (sa[i] - sb[i]).abs() < 1e-12,
                "nondeterminism at {i}: {} vs {}",
                sa[i],
                sb[i]
            );
        }
    }
}