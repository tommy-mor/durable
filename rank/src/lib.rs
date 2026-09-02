//! Rank centrality, compaction planning, and being projection.
//!
//! Pure: no I/O, no RNG from the OS. Pair sampling takes a seed.

pub mod plan;
pub mod project;
pub mod ranking;
pub mod text;

pub use plan::{finalize, nothing_to_compact, plan_pairs, CompactionPlan, CompactionResult};
pub use project::{project_json, Event, Projection};
pub use ranking::{
    connected_components, rank_from_comparisons, ranked_items_from_comparisons, Comparison,
    RankedItem,
};
pub use text::{before, cut, has_substr, parse_score, strip_tags};
