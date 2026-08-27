//! The full pipeline, asserted: .hop source → hopc → Lua → three Luau VMs
//! exchanging serialized packets → transcript.

use hoprt::compiler;
use hoprt::harness::Host;

fn run(app_code: &str) -> Vec<String> {
    let host = Host::new(&["A", "B"], app_code, false).expect("host");
    host.fire("A", "demo_join").unwrap();
    host.fire("B", "demo_join").unwrap();
    host.pump().unwrap();
    host.fire("A", "demo_flows").unwrap();
    host.pump().unwrap();
    host.fire("B", "demo_stroke").unwrap();
    host.pump().unwrap();
    host.assert_quiescent().unwrap();
    host.log()
}

fn assert_transcript(log: &[String]) {
    let all = log.join("\n");
    let has = |needle: &str| {
        assert!(all.contains(needle), "transcript missing {needle:?}:\n{all}");
    };

    // sessions joined with server-assigned colors
    has("session A joins as tomato");
    has("session B joins as steelblue");

    // round trips: values crossed as data
    has("-> 'carol' is available");
    has("-> 'alice' is taken");

    // server-side error propagated to the origin flow
    has("reserved handle");

    // chained hops with pass-through liveness (n through the confirm hop)
    has("[dialog] really delete 3 items?");
    has("purged data for session A");
    has("account: deleted 3 items");

    // broadcast reached both tabs with server-stamped color
    has("[browser A] draw (1,1) -> (2,3) in steelblue");
    has("[browser B] draw (1,1) -> (2,3) in steelblue");
}

#[test]
fn hand_compiled_app_runs() {
    let app = include_str!("../lua/app.lua");
    // app.lua's transcript differs slightly in wording; assert the shared core
    let log = run(app);
    let all = log.join("\n");
    assert!(all.contains("'carol' is available"), "{all}");
    assert!(all.contains("reserved handle"), "{all}");
    assert!(all.contains("deleted 3 items"), "{all}");
}

#[test]
fn hopc_compiled_hop_runs() {
    let src = include_str!("../hop/demo.hop");
    let lua = compiler::compile(src).expect("hopc compile");
    let log = run(&lua);
    assert_transcript(&log);
}

#[test]
fn liveness_ships_only_what_the_remainder_uses() {
    let src = include_str!("../hop/demo.hop");
    let lua = compiler::compile(src).expect("hopc compile");
    // check_handle hop 1 ships handle; hop 2 ships ok plus handle, which is
    // still live because the result line echoes it
    assert!(lua.contains(r#"rt.at("server", "check_handle:1", { handle = handle })"#), "{lua}");
    assert!(lua.contains(r#"rt.at(rt.session(), "check_handle:2", { handle = handle, ok = ok })"#), "{lua}");
    // delete_account chain: {} → {n} → {n, yes} → {msg} — n and yes are
    // dead after the last server segment and dropped from the final hop
    assert!(lua.contains(r#"rt.at("server", "delete_account:1", {  })"#), "{lua}");
    assert!(lua.contains(r#"rt.at(rt.session(), "delete_account:2", { n = n })"#), "{lua}");
    assert!(lua.contains(r#"rt.at("server", "delete_account:3", { n = n, yes = yes })"#), "{lua}");
    assert!(lua.contains(r#"rt.at(rt.session(), "delete_account:4", { msg = msg })"#), "{lua}");
    // nested casts: outer captures {from, to}; inner adds color
    assert!(lua.contains(r#"rt.cast("server", "stroke:c1", { from = from, to = to })"#), "{lua}");
    assert!(lua.contains(r#"rt.cast("browsers", "stroke:c2", { color = color, from = from, to = to })"#), "{lua}");
}

#[test]
fn todo_app_runs_on_simulated_cluster() {
    let src = include_str!("../hop/todo.hop");
    let lua = compiler::compile(src).expect("hopc compile");
    // the simulated cluster has no DOM; stub it with a print
    let dom_stub =
        "dom = { set = function(sel, html) print(\"[dom] \" .. sel .. \" := \" .. html) end }\n";
    let code = format!("{dom_stub}{lua}");

    let host = Host::new(&["A", "B"], &code, false).expect("host");
    host.fire("A", "sim_demo").unwrap();
    host.pump().unwrap();

    // "Click" the first todo on tab A. Handler ids are deterministic here:
    // render 1 minted id 1 (one item), render 2 released it and minted 2, 3
    // — so id 2 is the first <li>'s onclick closure, which captured i = 1.
    host.fire_with("A", "__handler_fire", 2).unwrap();
    host.pump().unwrap();
    host.assert_quiescent().unwrap();

    let all = host.log().join("\n");
    // final render on BOTH tabs: item 1 struck through, item 2 not.
    // (render 3 released ids 2 and 3, minted 4 and 5 — never reused.)
    let final_html = "<ul>\
                      <li class=\"done\" onclick=\"__hopHandler(4)\">buy milk</li>\
                      <li onclick=\"__hopHandler(5)\">write the compiler</li>\
                      </ul>";
    for tab in ["A", "B"] {
        assert!(
            all.contains(&format!("[browser {tab}] [dom] #todos := {final_html}")),
            "tab {tab} missing final render:\n{all}"
        );
    }
}

#[test]
fn lambda_with_marks_ships_only_its_captures() {
    let src = include_str!("../hop/todo.hop");
    let lua = compiler::compile(src).expect("hopc compile");
    // the onclick lambda hops to the server carrying only the loop index it
    // captured — not item, rows, or the items array
    assert!(lua.contains(r#"return rt.at("server", "todo_view:l1:1", { i = i })"#), "{lua}");
    // its server segment is registered like any other, and the cast inside
    // it carries the snapshot
    assert!(lua.contains(r#"rt.register("todo_view:l1:1""#), "{lua}");
    assert!(lua.contains(r#"rt.cast("browsers", "todo_view:c1", { snapshot = snapshot })"#), "{lua}");
}

#[test]
fn marks_rejected_inside_branches() {
    let src = "fn f(x) { if x { server!(); } }";
    let err = compiler::compile(src).unwrap_err();
    assert!(err.contains("top level"), "{err}");
}
