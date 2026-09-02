use std::collections::{HashMap, HashSet};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Comparison {
    pub a_id: String,
    pub b_id: String,
    pub score: i32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RankedItem {
    pub item: String,
    pub score: f64,
}

fn id_to_idx(memory_ids: &[String]) -> HashMap<&str, usize> {
    memory_ids
        .iter()
        .enumerate()
        .map(|(i, id)| (id.as_str(), i))
        .collect()
}

/// Build directed edge weights and voted-pair set from comparisons.
/// Positive score means `a_id` is preferred over `b_id`.
fn edges_from_comparisons(
    id_to_idx: &HashMap<&str, usize>,
    comparisons: &[Comparison],
) -> (HashMap<(usize, usize), f64>, HashSet<(usize, usize)>) {
    let mut edges: HashMap<(usize, usize), f64> = HashMap::new();
    let mut voted_pairs: HashSet<(usize, usize)> = HashSet::new();

    for c in comparisons {
        let Some(&i) = id_to_idx.get(c.a_id.as_str()) else {
            continue;
        };
        let Some(&j) = id_to_idx.get(c.b_id.as_str()) else {
            continue;
        };
        let (lo, hi) = if i < j { (i, j) } else { (j, i) };
        voted_pairs.insert((lo, hi));

        let p_a = (c.score + 50) as f64 / 100.0;
        *edges.entry((j, i)).or_insert(0.0) += p_a;
        *edges.entry((i, j)).or_insert(0.0) += 1.0 - p_a;
    }

    (edges, voted_pairs)
}

/// Compute connected components over the voted-pairs graph (treated as undirected).
///
/// Returns:
/// - `components`: each component is a sorted list of node indices, excluding isolates.
/// - `isolates`: sorted list of node indices with degree 0 (no voted pairs).
pub fn connected_components_from_voted_pairs(
    n: usize,
    voted_pairs: impl Iterator<Item = (usize, usize)>,
) -> (Vec<Vec<usize>>, Vec<usize>) {
    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (a, b) in voted_pairs {
        if a >= n || b >= n || a == b {
            continue;
        }
        adj[a].push(b);
        adj[b].push(a);
    }

    let mut isolates: Vec<usize> = (0..n).filter(|&i| adj[i].is_empty()).collect();
    isolates.sort();

    let mut seen = vec![false; n];
    for &i in &isolates {
        seen[i] = true;
    }

    let mut comps: Vec<Vec<usize>> = Vec::new();
    for i in 0..n {
        if seen[i] {
            continue;
        }
        let mut stack = vec![i];
        seen[i] = true;
        let mut comp: Vec<usize> = Vec::new();
        while let Some(x) = stack.pop() {
            comp.push(x);
            for &y in &adj[x] {
                if !seen[y] {
                    seen[y] = true;
                    stack.push(y);
                }
            }
        }
        comp.sort();
        comps.push(comp);
    }

    (comps, isolates)
}

pub fn connected_components(
    memory_ids: &[String],
    comparisons: &[Comparison],
) -> (Vec<Vec<usize>>, Vec<usize>) {
    let id_to_idx = id_to_idx(memory_ids);
    let (_, voted_pairs) = edges_from_comparisons(&id_to_idx, comparisons);
    connected_components_from_voted_pairs(memory_ids.len(), voted_pairs.into_iter())
}

/// Build comparison matrix and return memory ids sorted by rank descending.
pub fn rank_from_comparisons(memory_ids: &[String], comparisons: &[Comparison]) -> Vec<String> {
    ranked_items_from_comparisons(memory_ids, comparisons, 100_000, 1e-8)
        .into_iter()
        .map(|r| r.item)
        .collect()
}

pub fn ranked_items_from_comparisons(
    memory_ids: &[String],
    comparisons: &[Comparison],
    max_iters: usize,
    tol: f64,
) -> Vec<RankedItem> {
    if memory_ids.is_empty() {
        return vec![];
    }
    if memory_ids.len() == 1 {
        return vec![RankedItem {
            item: memory_ids[0].clone(),
            score: 1.0,
        }];
    }

    let id_to_idx = id_to_idx(memory_ids);
    let (_, voted_pairs) = edges_from_comparisons(&id_to_idx, comparisons);
    let (components, isolates) =
        connected_components_from_voted_pairs(memory_ids.len(), voted_pairs.into_iter());

    // RC only applies to connected graphs; rank each component independently.
    let mut grouped: Vec<Vec<RankedItem>> = Vec::new();
    let mut isolates_ranked: Vec<RankedItem> = Vec::new();

    for comp in components {
        debug_assert!(comp.len() > 1);
        grouped.push(ranked_items_subset(
            memory_ids,
            comparisons,
            &comp,
            max_iters,
            tol,
        ));
    }

    for &i in &isolates {
        isolates_ranked.push(RankedItem {
            item: memory_ids[i].clone(),
            score: 0.0,
        });
    }

    // Scores are only comparable within a component; order connected groups by top score.
    grouped.sort_by(|a, b| {
        b[0].score
            .partial_cmp(&a[0].score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut items: Vec<RankedItem> = grouped.into_iter().flatten().collect();
    items.extend(isolates_ranked);
    items
}

fn compute_scores_from_edges(
    n: usize,
    edges: impl Iterator<Item = ((usize, usize), f64)>,
    max_iters: usize,
    tol: f64,
) -> Vec<f64> {
    if n == 0 {
        return vec![];
    }
    if n == 1 {
        return vec![1.0];
    }

    let mut raw: HashMap<(usize, usize), f64> = HashMap::new();
    for ((src, dst), w) in edges {
        if src >= n || dst >= n || w <= 0.0 {
            continue;
        }
        *raw.entry((src, dst)).or_insert(0.0) += w;
    }

    let keys: Vec<(usize, usize)> = raw.keys().copied().collect();
    let mut normalized: HashMap<(usize, usize), f64> = HashMap::new();
    for (i, j) in keys {
        if normalized.contains_key(&(i, j)) {
            continue;
        }
        let w_ij = *raw.get(&(i, j)).unwrap_or(&0.0);
        let w_ji = *raw.get(&(j, i)).unwrap_or(&0.0);
        let total = w_ij + w_ji;
        if total <= 0.0 {
            continue;
        }
        normalized.insert((i, j), w_ij / total);
        if w_ji > 0.0 {
            normalized.insert((j, i), w_ji / total);
        }
    }

    // Rank Centrality (Negahban, Oh, Shah 2012, §3.1):
    //   P_ij = (1/d_max) * A_ij           for i ≠ j compared
    //   P_ii = 1 - (1/d_max) * Σ_k A_ik
    // where d_i is the *degree* (number of distinct neighbors compared) and
    // d_max = max_i d_i. Using the unweighted degree — not the sum of
    // pairwise-normalized weights — is what guarantees aperiodicity.
    let mut out_edges: Vec<Vec<(usize, f64)>> = vec![Vec::new(); n];
    let mut neighbors: Vec<HashSet<usize>> = vec![HashSet::new(); n];

    for ((src, dst), w) in &normalized {
        out_edges[*src].push((*dst, *w));
        neighbors[*src].insert(*dst);
        neighbors[*dst].insert(*src);
    }

    let weight_sum: Vec<f64> = out_edges
        .iter()
        .map(|es| es.iter().map(|(_, w)| *w).sum())
        .collect();
    let d_max = neighbors.iter().map(|s| s.len()).max().unwrap_or(0);
    if d_max == 0 {
        return vec![1.0 / n as f64; n];
    }
    let d_max_f = d_max as f64;

    let mut scores = vec![1.0 / n as f64; n];
    let mut next = vec![0.0f64; n];

    for _ in 0..max_iters {
        next.fill(0.0);
        for i in 0..n {
            let stay_prob = (d_max_f - weight_sum[i]) / d_max_f;
            next[i] += scores[i] * stay_prob;

            if out_edges[i].is_empty() {
                continue;
            }
            for &(dst, w) in &out_edges[i] {
                next[dst] += scores[i] * (w / d_max_f);
            }
        }

        let diff: f64 = scores
            .iter()
            .zip(next.iter())
            .map(|(a, b)| (a - b).abs())
            .sum();

        scores.clone_from_slice(&next);
        if diff < tol {
            break;
        }
    }

    let sum: f64 = scores.iter().sum();
    if sum.is_finite() && sum > 0.0 {
        for s in &mut scores {
            *s /= sum;
        }
    }
    scores
}

/// Rank-centrality within a subset of items (an induced subgraph).
pub fn ranked_items_subset(
    memory_ids: &[String],
    comparisons: &[Comparison],
    idxs: &[usize],
    max_iters: usize,
    tol: f64,
) -> Vec<RankedItem> {
    if idxs.is_empty() {
        return vec![];
    }

    let id_to_idx = id_to_idx(memory_ids);
    let (edges, _) = edges_from_comparisons(&id_to_idx, comparisons);

    let mut map: HashMap<usize, usize> = HashMap::with_capacity(idxs.len());
    for (j, &i) in idxs.iter().enumerate() {
        map.insert(i, j);
    }

    let edges_iter = edges.into_iter().filter_map(|((src, dst), w)| {
        let s = *map.get(&src)?;
        let d = *map.get(&dst)?;
        Some(((s, d), w))
    });

    let scores = compute_scores_from_edges(idxs.len(), edges_iter, max_iters, tol);

    let mut items: Vec<RankedItem> = idxs
        .iter()
        .enumerate()
        .filter_map(|(j, &orig)| {
            let item = memory_ids.get(orig)?.clone();
            Some(RankedItem {
                item,
                score: *scores.get(j).unwrap_or(&0.0),
            })
        })
        .collect();

    items.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmp(a: &str, b: &str, score: i32) -> Comparison {
        Comparison {
            a_id: a.to_string(),
            b_id: b.to_string(),
            score,
        }
    }

    /// Score equivalent to a 2:1 preference for `a` over `b`.
    fn ratio_2_1(a: &str, b: &str) -> Comparison {
        cmp(a, b, 17)
    }

    #[test]
    fn single_item() {
        assert_eq!(rank_from_comparisons(&["a".into()], &[]), vec!["a"]);
    }

    #[test]
    fn clear_winner() {
        let ids = vec!["a".into(), "b".into()];
        let comps = vec![cmp("a", "b", 50)];
        let ranked = rank_from_comparisons(&ids, &comps);
        assert_eq!(ranked[0], "a");
    }

    /// Regression: pure forward star at 2:1 preference. Degree-based d_max
    /// gives every node a positive self-loop so the chain converges.
    #[test]
    fn star_topology_winner_at_top_via_subset() {
        let ids: Vec<String> = vec!["alpha".into(), "beta".into(), "zebra".into()];
        let comparisons = vec![ratio_2_1("zebra", "alpha"), ratio_2_1("zebra", "beta")];

        let mut items: Vec<(usize, String)> = ids
            .iter()
            .enumerate()
            .map(|(i, id)| (i, id.clone()))
            .collect();
        items.sort_by(|a, b| a.1.cmp(&b.1));
        let idxs: Vec<usize> = items.iter().map(|(i, _)| *i).collect();

        let ranked = ranked_items_subset(&ids, &comparisons, &idxs, 10000, 1e-8);
        assert_eq!(
            ranked[0].item, "zebra",
            "zebra won both votes and should rank #1"
        );
    }

    #[test]
    fn connected_components_split_disconnected_pairs() {
        let ids = vec!["a".into(), "b".into(), "c".into(), "d".into()];
        let comparisons = vec![cmp("a", "b", 50), cmp("c", "d", 50)];

        let (mut comps, isolates) = connected_components(&ids, &comparisons);
        assert!(isolates.is_empty());
        comps.sort_by_key(|c| c.iter().map(|&i| ids[i].clone()).collect::<Vec<_>>());
        assert_eq!(comps.len(), 2);
        let comp0: Vec<_> = comps[0].iter().map(|&i| ids[i].as_str()).collect();
        let comp1: Vec<_> = comps[1].iter().map(|&i| ids[i].as_str()).collect();
        assert_eq!(comp0, vec!["a", "b"]);
        assert_eq!(comp1, vec!["c", "d"]);
    }

    #[test]
    fn subset_ranking_ranks_within_component_only() {
        let ids = vec!["a".into(), "b".into(), "c".into(), "d".into()];
        let comparisons = vec![
            cmp("a", "b", 50),  // a > b
            cmp("c", "d", -50), // d > c
        ];

        let (comps, _) = connected_components(&ids, &comparisons);
        assert_eq!(comps.len(), 2);

        for comp in comps {
            let ranked = ranked_items_subset(&ids, &comparisons, &comp, 10000, 1e-8);
            assert_eq!(ranked.len(), 2);
            let names: Vec<_> = ranked.iter().map(|r| r.item.as_str()).collect();
            if names.contains(&"a") {
                assert_eq!(names[0], "a");
            } else {
                assert_eq!(names[0], "d");
            }
        }
    }

    #[test]
    fn rank_from_comparisons_ranks_each_component_separately() {
        let ids = vec![
            "a".into(),
            "b".into(),
            "c".into(),
            "d".into(),
            "loner".into(),
        ];
        let comparisons = vec![
            cmp("a", "b", 50),  // a > b
            cmp("c", "d", -50), // d > c
        ];

        let ranked = rank_from_comparisons(&ids, &comparisons);
        let pos = |id: &str| ranked.iter().position(|x| x == id).unwrap();

        assert!(pos("a") < pos("b"));
        assert!(pos("d") < pos("c"));
        assert_eq!(pos("loner"), 4);
    }
}
