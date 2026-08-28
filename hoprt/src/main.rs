//! hoprt demo host: run the hop runtime demo on a simulated cluster.
//!
//! ```text
//! cargo run -p hoprt                     # hop/demo.hop
//! cargo run -p hoprt -- hop/chat.hop     # any .hop app with the demo entry points
//! ```

use hoprt::harness::Cluster;

fn main() {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| format!("{}/hop/demo.hop", env!("CARGO_MANIFEST_DIR")));
    let src = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));

    let mut host = Cluster::new(&["A", "B"], &src, true).unwrap_or_else(|e| panic!("{path}: {e}"));

    host.banner("== phase 1: sessions join =========================================");
    host.fire("A", "demo_join");
    host.fire("B", "demo_join");
    host.pump();

    host.banner("");
    host.banner("== phase 2: browser A fires four flows ============================");
    host.banner("   (round trip, round trip, server error, nested/chained hops)");
    host.fire("A", "demo_flows");
    host.pump();

    host.banner("");
    host.banner("== phase 3: browser B draws; broadcast reaches A and B ============");
    host.fire("B", "demo_stroke");
    host.pump();

    host.banner("");
    host.assert_quiescent();
    println!("done: queue drained and every VM is quiescent — no leaked flows");
}
