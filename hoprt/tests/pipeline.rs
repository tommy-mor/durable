//! The full pipeline, asserted: .hop source → hopc → Hop IR → three native
//! VMs exchanging CBOR-encoded packets → transcript. Assertions are on
//! packets, transcripts, and rendered output — never on VM internals.

use hoprt::harness::Cluster;

fn run(src: &str) -> Vec<String> {
    let mut host = Cluster::new(&["A", "B"], src, false).expect("cluster");
    host.fire("A", "demo_join");
    host.fire("B", "demo_join");
    host.pump();
    host.fire("A", "demo_flows");
    host.pump();
    host.fire("B", "demo_stroke");
    host.pump();
    host.assert_quiescent();
    host.log()
}

#[test]
fn demo_app_transcript() {
    let log = run(include_str!("../hop/demo.hop"));
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
fn liveness_ships_only_what_the_remainder_uses() {
    // ship sets are asserted on the wire itself: the vars map of each call
    // and cast packet, in diagnostic notation (map keys are ordered).
    let log = run(include_str!("../hop/demo.hop"));
    let all = log.join("\n");
    let has = |needle: &str| {
        assert!(all.contains(needle), "wire missing {needle:?}:\n{all}");
    };

    // check_handle hop 1 ships handle; hop 2 ships ok plus handle, which
    // is still live because the result line echoes it
    has(r#"check_handle:1 vars={handle: "carol"}"#);
    has(r#"check_handle:2 vars={handle: "carol", ok: true}"#);

    // delete_account chain: {} → {n} → {n, yes} → {msg} — n and yes are
    // dead after the last server segment and dropped from the final hop
    has("delete_account:1 vars={}");
    has("delete_account:2 vars={n: 3}");
    has("delete_account:3 vars={n: 3, yes: true}");
    has(r#"delete_account:4 vars={msg: "deleted 3 items"}"#);

    // nested casts: outer captures {from, to}; inner adds the server-side
    // color
    has(r#"stroke:c1 vars={from: "(1,1)", to: "(2,3)"}"#);
    has(r#"stroke:c2 vars={color: "steelblue", from: "(1,1)", to: "(2,3)"}"#);
}

#[test]
fn todo_app_runs_on_simulated_cluster() {
    let src = include_str!("../hop/todo.hop");
    let mut host = Cluster::new(&["A", "B"], src, false).expect("cluster");
    host.fire("A", "sim_demo");
    host.pump();

    // Renders go to #app. After two adds the first item's onclick is
    // handler 4 (render 1 mints button=1, milk=2; render 2 mints button=3,
    // milk=4, compiler=5).
    host.fire_handler("A", 4);
    host.pump();
    host.assert_quiescent();

    for tab in ["A", "B"] {
        let app = host.dom(tab, "#app");
        assert!(app.contains("buy milk"), "tab {tab}: {app}");
        assert!(app.contains("write the compiler"), "tab {tab}: {app}");
        assert!(app.contains("class=\"done\""), "tab {tab} missing struck-through milk: {app}");
        assert!(app.contains("1 done of 2"), "tab {tab} stats: {app}");
    }
}

#[test]
fn lambda_with_marks_ships_only_its_captures() {
    // the onclick lambda hops to the server carrying only the id it
    // captured — not item, rows, or the items array
    let src = include_str!("../hop/todo.hop");
    let mut host = Cluster::new(&["A"], src, false).expect("cluster");
    host.fire("A", "sim_demo");
    host.pump();
    host.fire_handler("A", 4);
    host.pump();
    host.assert_quiescent();

    let all = host.log().join("\n");
    assert!(all.contains("todo_view:l1:1 vars={id: 0}"), "{all}");
    // its cast carries the snapshot back out
    assert!(all.contains("todo_view:c1 vars={completed: 1, created: 2, snapshot:"), "{all}");
}

#[test]
fn map_iteration_is_deterministic_key_order() {
    let src = r#"
        fn go() {
          let m = { b = 2, a = 1, c = 3 };
          for k, v in m {
            print(k .. "=" .. v);
          }
        }
    "#;
    let mut host = Cluster::new(&["A"], src, false).expect("cluster");
    host.fire("A", "go");
    host.pump();
    let all = host.log().join("\n");
    let a = all.find("a=1").expect("a=1");
    let b = all.find("b=2").expect("b=2");
    let c = all.find("c=3").expect("c=3");
    assert!(a < b && b < c, "map iteration out of key order:\n{all}");
}

#[test]
fn arrays_are_zero_based() {
    let src = r#"
        fn go() {
          let xs = ["first", "second"];
          print(xs[0] .. "/" .. tostring(xs[2]));
          for i, x in xs {
            print(i .. ":" .. x);
          }
        }
    "#;
    let mut host = Cluster::new(&["A"], src, false).expect("cluster");
    host.fire("A", "go");
    host.pump();
    let all = host.log().join("\n");
    assert!(all.contains("first/nil"), "{all}");
    assert!(all.contains("0:first"), "{all}");
    assert!(all.contains("1:second"), "{all}");
}

#[test]
fn marks_rejected_inside_branches() {
    let src = "fn f(x) { if x { server!(); } }";
    let err = match hoprt::compiler::compile(src) {
        Err(e) => e,
        Ok(_) => panic!("nested mark compiled"),
    };
    assert!(err.contains("top level"), "{err}");
}

#[test]
fn marks_rejected_inside_while() {
    let src = "fn f(x) { while x { server!(); } }";
    let err = match hoprt::compiler::compile(src) {
        Err(e) => e,
        Ok(_) => panic!("mark in while compiled"),
    };
    assert!(err.contains("top level"), "{err}");
}

#[test]
fn while_loops_run() {
    let src = r#"
        fn go() {
          let n = 3;
          let acc = "";
          while n > 0 {
            acc = acc .. n;
            n = n - 1;
          }
          print("acc " .. acc);
          while false {
            print("never");
          }
        }
    "#;
    let mut host = Cluster::new(&["A"], src, false).expect("cluster");
    host.fire("A", "go");
    host.pump();
    let all = host.log().join("\n");
    assert!(all.contains("acc 321"), "{all}");
    assert!(!all.contains("never"), "{all}");
}

#[test]
fn json_and_type_natives() {
    let src = r#"
        fn go() {
          let v = json.decode("{\"a\":1,\"b\":[true,null]}");
          print(type(v) .. " a=" .. v.a);
          print(json.encode(v));
          print("bad=" .. type(json.decode("not json")));
          print("scalar=" .. type(json.decode("42")));
        }
    "#;
    let mut host = Cluster::new(&["A"], src, false).expect("cluster");
    host.fire("A", "go");
    host.pump();
    let all = host.log().join("\n");
    assert!(all.contains("map a=1"), "{all}");
    assert!(all.contains(r#"{"a":1,"b":[true,null]}"#), "{all}");
    assert!(all.contains("bad=nil"), "{all}");
    assert!(all.contains("scalar=int"), "{all}");
}

#[test]
fn match_dispatches_on_type_and_destructures() {
    let src = r#"
        fn handle(event) {
          match event {
            add { text, n } => {
              print("add " .. text .. " x" .. n);
            }
            toggle { id } => {
              // the subject stays in scope alongside the destructured fields
              print("toggle " .. id .. " (" .. event.type .. ")");
            }
          }
        }
        fn go() {
          handle({ type = "add", text = "milk", n = 2 });
          handle({ type = "toggle", id = 7 });
        }
    "#;
    let mut host = Cluster::new(&["A"], src, false).expect("cluster");
    host.fire("A", "go");
    host.pump();
    host.assert_quiescent();
    let all = host.log().join("\n");
    assert!(all.contains("add milk x2"), "{all}");
    assert!(all.contains("toggle 7 (toggle)"), "{all}");
}

#[test]
fn match_else_arm_and_no_match_fall_through() {
    let src = r#"
        fn classify(event) {
          match event {
            known => { print("known"); }
            else => { print("other " .. event.type); }
          }
          // no matching arm and no else: the whole match is a no-op
          match event {
            never => { print("never"); }
          }
          print("after " .. event.type);
        }
        fn go() {
          classify({ type = "known" });
          classify({ type = "mystery" });
        }
    "#;
    let mut host = Cluster::new(&["A"], src, false).expect("cluster");
    host.fire("A", "go");
    host.pump();
    host.assert_quiescent();
    let all = host.log().join("\n");
    assert!(all.contains("known"), "{all}");
    assert!(all.contains("other mystery"), "{all}");
    assert!(all.contains("after known"), "{all}");
    assert!(all.contains("after mystery"), "{all}");
    assert!(!all.contains("never"), "unmatched arm ran:\n{all}");
}

#[test]
fn marks_rejected_inside_match_arms() {
    let src = "fn f(e) { match e { go => { server!(); } } }";
    let err = match hoprt::compiler::compile(src) {
        Err(e) => e,
        Ok(_) => panic!("mark inside match arm compiled"),
    };
    assert!(err.contains("match arms"), "{err}");
    assert!(err.contains("top level"), "{err}");
}
