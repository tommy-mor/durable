use std::collections::HashSet;

use crate::ranking::{ranked_items_from_comparisons, Comparison};

/// Seeded LCG so pair planning is deterministic and wasm-safe (no getrandom).
struct Lcg(u64);

impl Lcg {
    fn next_u32(&mut self) -> u32 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1);
        (self.0 >> 32) as u32
    }

    fn index(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next_u32() as usize) % n
        }
    }

    fn choose<'a, T>(&mut self, items: &'a [T]) -> Option<&'a T> {
        if items.is_empty() {
            None
        } else {
            Some(&items[self.index(items.len())])
        }
    }

    fn choose_two<'a, T>(&mut self, items: &'a [T]) -> Option<(&'a T, &'a T)> {
        if items.len() < 2 {
            return None;
        }
        let i = self.index(items.len());
        let mut j = self.index(items.len() - 1);
        if j >= i {
            j += 1;
        }
        Some((&items[i], &items[j]))
    }
}

fn sorted_pair(a: &str, b: &str) -> (String, String) {
    if a < b {
        (a.to_string(), b.to_string())
    } else {
        (b.to_string(), a.to_string())
    }
}

fn add_pair(
    pairs: &mut Vec<(String, String)>,
    known_pairs: &mut HashSet<(String, String)>,
    a: String,
    b: String,
) {
    if a == b {
        return;
    }
    let key = sorted_pair(&a, &b);
    if known_pairs.insert(key) {
        pairs.push((a, b));
    }
}

/// Union-find components over string ids (ember's `find_components`).
fn find_components(nodes: &HashSet<String>, edges: &[(String, String)]) -> Vec<Vec<String>> {
    let mut parent: std::collections::HashMap<&str, &str> =
        nodes.iter().map(|n| (n.as_str(), n.as_str())).collect();

    fn find<'a>(parent: &mut std::collections::HashMap<&'a str, &'a str>, x: &'a str) -> &'a str {
        if parent[x] != x {
            let root = find(parent, parent[x]);
            parent.insert(x, root);
        }
        parent[x]
    }

    fn union<'a>(parent: &mut std::collections::HashMap<&'a str, &'a str>, x: &'a str, y: &'a str) {
        let rx = find(parent, x);
        let ry = find(parent, y);
        if rx != ry {
            parent.insert(rx, ry);
        }
    }

    for (a, b) in edges {
        if nodes.contains(a) && nodes.contains(b) {
            union(&mut parent, a, b);
        }
    }

    let mut components: std::collections::HashMap<&str, Vec<String>> =
        std::collections::HashMap::new();
    for n in nodes {
        let root = find(&mut parent, n);
        components.entry(root).or_default().push(n.clone());
    }
    for c in components.values_mut() {
        c.sort();
    }
    components.into_values().collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionPlan {
    pub pairs: Vec<(String, String)>,
}

/// New vote pairs: bridge disconnected vote-graph components, then sample
/// random pairs. `seed` makes the sample reproducible.
pub fn plan_pairs(ids: &[String], comparisons: &[Comparison], seed: u64) -> CompactionPlan {
    let all_ids: HashSet<String> = ids.iter().cloned().collect();
    let mut known_pairs = HashSet::new();
    let mut existing_pairs = Vec::new();
    for c in comparisons {
        if all_ids.contains(&c.a_id) && all_ids.contains(&c.b_id) {
            let key = sorted_pair(&c.a_id, &c.b_id);
            if known_pairs.insert(key.clone()) {
                existing_pairs.push((c.a_id.clone(), c.b_id.clone()));
            }
        }
    }

    let components = find_components(&all_ids, &existing_pairs);
    let mut new_pairs: Vec<(String, String)> = Vec::new();
    let mut rng = Lcg(seed | 1);

    if components.len() > 1 {
        let mut comps: Vec<Vec<String>> = components.into_iter().filter(|c| !c.is_empty()).collect();
        comps.sort_by(|a, b| a.len().cmp(&b.len()).then_with(|| a[0].cmp(&b[0])));
        if let Some(mut main) = comps.pop() {
            for comp in comps {
                let Some(a) = rng.choose(&main).cloned() else {
                    continue;
                };
                let Some(b) = rng.choose(&comp).cloned() else {
                    continue;
                };
                add_pair(&mut new_pairs, &mut known_pairs, a, b);
                main.extend(comp);
            }
        }
    }

    let num_random = (all_ids.len() / 5).max(20);
    let mut candidate_list: Vec<String> = all_ids.into_iter().collect();
    candidate_list.sort();
    for _ in 0..num_random {
        let Some((a, b)) = rng.choose_two(&candidate_list) else {
            break;
        };
        add_pair(
            &mut new_pairs,
            &mut known_pairs,
            a.clone(),
            b.clone(),
        );
    }

    CompactionPlan { pairs: new_pairs }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionResult {
    pub kept: Vec<String>,
    pub released: Vec<String>,
}

/// Rank every memory, keep the top `budget`, release whatever is currently
/// held and didn't make the cut. Previously released memories can return
/// if the budget grows.
pub fn finalize(
    all_ids: &[String],
    current_ids: &[String],
    comparisons: &[Comparison],
    budget: usize,
) -> CompactionResult {
    let ranked = ranked_items_from_comparisons(all_ids, comparisons, 100_000, 1e-8);
    let kept: Vec<String> = ranked
        .into_iter()
        .take(budget.min(all_ids.len()))
        .map(|item| item.item)
        .collect();
    let used: HashSet<String> = kept.iter().cloned().collect();
    let current: HashSet<String> = current_ids.iter().cloned().collect();
    let mut released: Vec<String> = current.difference(&used).cloned().collect();
    released.sort();
    CompactionResult { kept, released }
}

/// Nothing to do when there are no memories, or everyone is already
/// current and the budget holds them all.
pub fn nothing_to_compact(all_len: usize, current_len: usize, budget: usize) -> bool {
    all_len == 0 || (current_len == all_len && budget >= all_len)
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

    #[test]
    fn nothing_when_already_within_budget() {
        assert!(nothing_to_compact(4, 4, 4));
        assert!(nothing_to_compact(0, 0, 10));
        assert!(!nothing_to_compact(8, 8, 4));
        assert!(!nothing_to_compact(8, 4, 10));
    }

    #[test]
    fn finalize_keeps_the_winner() {
        let ids = vec!["a".into(), "b".into(), "c".into()];
        let current = ids.clone();
        let comps = vec![cmp("a", "b", 50), cmp("a", "c", 50)];
        let out = finalize(&ids, &current, &comps, 1);
        assert_eq!(out.kept, vec!["a"]);
        assert_eq!(out.released, vec!["b".to_string(), "c".to_string()]);
    }

    #[test]
    fn plan_bridges_two_components() {
        let ids = vec!["a".into(), "b".into(), "c".into(), "d".into()];
        let comps = vec![cmp("a", "b", 50), cmp("c", "d", 50)];
        let plan = plan_pairs(&ids, &comps, 1);
        // at least one bridge pair plus random samples
        assert!(!plan.pairs.is_empty());
        let bridged = plan.pairs.iter().any(|(x, y)| {
            let left = (*x == "a" || *x == "b") && (*y == "c" || *y == "d");
            let right = (*x == "c" || *x == "d") && (*y == "a" || *y == "b");
            left || right
        });
        assert!(bridged, "expected a cross-component pair: {:?}", plan.pairs);
    }

    #[test]
    fn same_seed_same_pairs() {
        let ids: Vec<String> = (0..12).map(|i| format!("m{i}")).collect();
        let a = plan_pairs(&ids, &[], 42);
        let b = plan_pairs(&ids, &[], 42);
        assert_eq!(a.pairs, b.pairs);
    }
}
