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

    // check_handle hops once with handle; ok comes BACK in the reply
    // wrapper — handle never leaves the parked browser frame, so there is
    // no second hop at all. (handle rides the reply too since the
    // mutation fix: anything that crossed and is still needed comes home —
    // the continuation echoes it.)
    has(r#"check_handle:1 vars={handle: "carol"}"#);
    has(r#"reply value={vars: {handle: "carol", ok: true}}"#);
    assert!(
        !all.contains("check_handle:2"),
        "the park/resume model collapsed the old chain:\n{all}"
    );

    // delete_account: two hops, not four — {} in (n is defined inside,
    // nothing dead ships), {n} back by reply, exactly {n, yes} into the
    // middle piece, {msg} back by reply
    has("delete_account:1 vars={}");
    has("reply value={vars: {n: 3}}");
    has("delete_account:2 vars={n: 3, yes: true}");
    has(r#"reply value={vars: {msg: "deleted 3 items"}}"#);
    assert!(
        !all.contains("delete_account:3"),
        "replies carry backward flow now, not chain hops:\n{all}"
    );

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
fn json_first_fishes_a_tool_call_out_of_prose() {
    let src = r#"
        fn go() {
          let messy = "Sure! ```json\n{\"tool\":\"bash\",\"cmd\":\"ls\"}\n``` and also {\"tool\":\"bash\",\"cmd\":\"pwd\"} trailing prose";
          let call = json.first(messy);
          print("tool=" .. call.tool .. " cmd=" .. call.cmd);
          print("none=" .. type(json.first("no json here { not json either")));
        }
    "#;
    let mut host = Cluster::new(&["A"], src, false).expect("cluster");
    host.fire("A", "go");
    host.pump();
    let all = host.log().join("\n");
    assert!(all.contains("tool=bash cmd=ls"), "{all}");
    assert!(all.contains("none=nil"), "{all}");
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
fn module_natives_mount_as_globals() {
    // The battery modules (modules.rs) mount by name: bare names become
    // globals, dotted names become fields on a module map. llm is a
    // plain map now — the one-shot call is llm.call(req).
    let src = r#"
        fn go() {
          print("json=" .. type(json) .. " first=" .. type(json.first));
          print("llm=" .. type(llm) .. " stream=" .. type(llm.stream) .. " call=" .. type(llm.call));
          print("bash=" .. type(bash) .. " markdown=" .. type(markdown));
          print("str=" .. type(str) .. " has=" .. type(str.has));
        }
    "#;
    let mut host = Cluster::new(&["A"], src, false).expect("cluster");
    host.fire("A", "go");
    host.pump();
    let all = host.log().join("\n");
    assert!(all.contains("json=map first=native"), "{all}");
    assert!(all.contains("llm=map stream=native call=native"), "{all}");
    assert!(all.contains("bash=native markdown=native"), "{all}");
    assert!(all.contains("str=map has=native"), "{all}");
}

#[test]
fn str_natives_cut_and_strip() {
    let src = r#"
        fn go() {
          if str.has("x !declaration y", "!declaration") { print("decl=yes"); }
          print("before=" .. str.before("aa [Response] bb", "[Response]"));
          print("strip=" .. str.strip("<thought>hi</thought>"));
        }
    "#;
    let mut host = Cluster::new(&["A"], src, false).expect("cluster");
    host.fire("A", "go");
    host.pump();
    let all = host.log().join("\n");
    assert!(all.contains("decl=yes"), "{all}");
    assert!(all.contains("before=aa"), "{all}");
    assert!(all.contains("strip=hi"), "{all}");
}

#[test]
fn markdown_renders_hiccup_with_escaping() {
    let src = r##"
        fn go() {
          hui.render("#x", markdown("# Title\n\nsome *em* and `code`\n\n- a\n- b\n\n```sh\nls -la\n```\n\n<script>alert(1)</script>"));
        }
    "##;
    let mut host = Cluster::new(&["A"], src, false).expect("cluster");
    host.fire("A", "go");
    host.pump();
    let html = host.dom("A", "#x");
    assert!(html.contains("<h1>Title</h1>"), "{html}");
    assert!(html.contains("<em>em</em>"), "{html}");
    assert!(html.contains("<code>code</code>"), "{html}");
    assert!(html.contains("<ul><li>a</li><li>b</li></ul>"), "{html}");
    assert!(html.contains(r#"<pre class="lang-sh">ls -la"#), "{html}");
    // raw HTML from the model is text, never markup
    assert!(!html.contains("<script>"), "{html}");
    assert!(html.contains("&lt;script&gt;"), "{html}");
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
fn marks_nest_inside_branches_loops_and_match_arms() {
    // The CFG pipeline's reason to exist: marks anywhere a flow body
    // reaches. The frame parks across each hop, so loop state, iterators,
    // and branch merges never cross the wire — placement prints prove
    // which side each line ran on.
    let src = r#"
        server let hits = 0;

        fn branch_mark(flag) {
          if flag {
            server!();
            hits = hits + 1;
            print("server branch, hits=" .. hits);
          }
          print("browser merge, flag=" .. flag);
        }

        fn while_mark() {
          let i = 0;
          while i < 3 {
            i = i + 1;
            server!();
            print("server while i=" .. i);
          }
          print("browser while done i=" .. i);
        }

        fn for_mark() {
          for k, v in ["a", "b"] {
            server!();
            print("server for " .. k .. "=" .. v);
          }
          print("browser for done");
        }

        fn match_mark(res) {
          match res {
            ok { value } => {
              server!();
              print("server match ok " .. value);
            }
            else => {
              print("browser match else");
            }
          }
        }

        // the reply wrapper: a local made server-side flows BACK into the
        // parked browser frame
        fn backward_var() {
          server!();
          let made = "server-made";
          browser!();
          print("browser got " .. made);
        }

        // a plain call to a fn whose body hops: the tail mark propagates
        // the far piece's return value into the caller's frame
        fn tail_val() {
          server!();
          return 42;
        }
        fn show_tail() {
          let v = tail_val();
          print("browser tail v=" .. v);
        }

        // the merge case: a hopped branch inside a loop, control rejoining
        // in the original frame each iteration
        fn merge_in_loop() {
          for k, v in [10, 20] {
            if v > 15 {
              server!();
              print("server merge sees " .. v);
            }
            print("browser tick " .. k);
          }
          print("browser merge done");
        }

        fn go() {
          spawn branch_mark(1);
          spawn while_mark();
          spawn for_mark();
          spawn match_mark({ type = "ok", value = "v7" });
          spawn match_mark({ type = "no" });
          spawn backward_var();
          spawn show_tail();
          spawn merge_in_loop();
        }
    "#;
    let mut host = Cluster::new(&["A"], src, false).expect("cluster");
    host.fire("A", "go");
    host.pump();
    host.assert_quiescent();
    let all = host.log().join("\n");
    let has = |needle: &str| assert!(all.contains(needle), "transcript missing {needle:?}:\n{all}");

    has("[server] server branch, hits=1");
    has("[browser A] browser merge, flag=1");
    has("[server] server while i=1");
    has("[server] server while i=3");
    has("[browser A] browser while done i=3");
    has("[server] server for 0=a");
    has("[server] server for 1=b");
    has("[browser A] browser for done");
    has("[server] server match ok v7");
    has("[browser A] browser match else");
    has("[browser A] browser got server-made");
    has("[browser A] browser tail v=42");
    has("[server] server merge sees 20");
    has("[browser A] browser tick 0");
    has("[browser A] browser tick 1");
    has("[browser A] browser merge done");
}

#[test]
fn casts_still_reject_marks() {
    // a cast is a fire-and-forget message whose body already runs on the
    // target — a hop inside it has no meaning
    let src = "fn f() { cast server { server!(); } }";
    let err = match hoprt::compiler::compile(src) {
        Err(e) => e,
        Ok(_) => panic!("mark inside cast body compiled"),
    };
    assert!(err.contains("cast bodies"), "{err}");
}

#[test]
fn mutation_across_a_hop_comes_home() {
    // regression (code review 2026-10-07): a var shipped into a piece is a
    // COPY on the far side; `push` mutates the copy but counts as a use,
    // not a def, so the old reply set (LiveIn ∩ defs) left mutations
    // stranded — the browser printed []. The reply now carries
    // LiveIn(cont) ∩ (defs ∪ ship): anything that crossed and is still
    // needed comes home.
    let src = r#"
        fn go() {
          let rows = [];
          server!();
          push(rows, 1);
          push(rows, 2);
          browser!();
          print("rows=" .. len(rows));
        }
    "#;
    let mut host = Cluster::new(&["A"], src, false).expect("cluster");
    host.fire("A", "go");
    host.pump();
    host.assert_quiescent();
    let all = host.log().join("\n");
    assert!(all.contains("rows=2"), "mutation lost across the hop:\n{all}");
    assert!(
        all.contains("rows: [1, 2]"),
        "reply wrapper didn't carry the mutated rows:\n{all}"
    );
}

#[test]
fn warns_when_live_var_is_out_of_scope_at_the_mark() {
    // q is defined only inside the branch, so it isn't in scope at the
    // mark and can't ship — the far side would silently read nil
    let src = "fn f(c) { if c { let q = 1; } server!(); print(q); }";
    let prog = hoprt::compiler::compile(src).expect("compile");
    let all = prog.warnings.join("\n");
    assert!(all.contains("f:1"), "no warning for the hop: {all}");
    assert!(all.contains("`q`"), "warning didn't name the var: {all}");
}

#[test]
fn explain_reports_what_crosses_each_hop() {
    // the lamp: hopc --explain's data, asserted at the Program level.
    // demo.hop's park/resume flow ships {handle} in and replies {ok}
    // back; the delete_account middle piece takes exactly {n, yes}.
    let prog = hoprt::compiler::compile(include_str!("../hop/demo.hop")).expect("compile");
    let all = prog.explain_listing();
    let has = |needle: &str| assert!(all.contains(needle), "explain missing {needle:?}:\n{all}");
    has("check_handle:1  side=server  ship={handle}  reply={handle, ok}");
    has("delete_account:1  side=server  ship={}  reply={n}");
    has("delete_account:2  side=server  ship={n, yes}  reply={msg}");
    has("stroke:c1  side=cast  ship={from, to}  reply={}");
    has("stroke:c2  side=cast  ship={color, from, to}  reply={}");
}
