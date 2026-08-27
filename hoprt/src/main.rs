//! hoprt demo host: run the hop runtime demo on a simulated cluster.
//!
//! By default runs the hand-compiled `lua/app.lua`. Pass a `.hop` file to
//! compile it with hopc and run the result instead:
//!
//! ```text
//! cargo run -p hoprt                     # hand-compiled app.lua
//! cargo run -p hoprt -- hop/demo.hop     # .hop → hopc → hoprt
//! ```

use hoprt::harness::Host;

fn main() -> mlua::Result<()> {
    let arg = std::env::args().nth(1);
    let app_code = match &arg {
        Some(path) if path.ends_with(".hop") => {
            let src = std::fs::read_to_string(path)
                .unwrap_or_else(|e| panic!("read {path}: {e}"));
            let lua = hoprt::compiler::compile(&src)
                .unwrap_or_else(|e| panic!("hopc: {path}: {e}"));
            println!("== compiled {path} with hopc ({} lines of Lua) ==\n", lua.lines().count());
            lua
        }
        Some(path) => {
            std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {path}: {e}"))
        }
        None => {
            let path = format!("{}/lua/app.lua", env!("CARGO_MANIFEST_DIR"));
            std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"))
        }
    };

    let host = Host::new(&["A", "B"], &app_code, true)?;

    host.banner("== phase 1: sessions join =========================================");
    host.fire("A", "demo_join")?;
    host.fire("B", "demo_join")?;
    host.pump()?;

    host.banner("");
    host.banner("== phase 2: browser A fires four flows ============================");
    host.banner("   (round trip, round trip, server error, nested/chained hops)");
    host.fire("A", "demo_flows")?;
    host.pump()?;

    host.banner("");
    host.banner("== phase 3: browser B draws; broadcast reaches A and B ============");
    host.fire("B", "demo_stroke")?;
    host.pump()?;

    host.banner("");
    host.assert_quiescent()?;
    println!("done: queue drained and every VM is quiescent — no leaked flows");
    Ok(())
}
