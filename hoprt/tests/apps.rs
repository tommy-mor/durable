//! Simulated-cluster tests for the larger hop apps. These prove hopc +
//! the durable store can run ranking (Sum edges + decay), microblog
//! (fan-out-on-write timelines), and chat (nested list under a map)
//! without a browser.

use hoprt::compiler;
use hoprt::harness::Host;

fn compile(src: &str) -> String {
    let lua = compiler::compile(src).expect("hopc compile");
    let dom = "dom = {\
        set = function(sel, html) print(\"[dom] \" .. sel .. \" := \" .. html) end,\
        get = function() return \"\" end,\
        clear = function() end\
    }\n";
    format!("{dom}{lua}")
}

#[test]
fn ranking_votes_and_decay() {
    let code = compile(include_str!("../hop/ranking.hop"));
    let host = Host::new(&["A", "B"], &code, false).expect("host");
    host.fire("A", "sim_demo").unwrap();
    host.pump().unwrap();
    host.assert_quiescent().unwrap();

    let votes: i64 = host
        .eval_server(r#"return store.one({"scopes", "tools", "votes"})"#)
        .unwrap();
    assert_eq!(votes, 3);

    let rg_grep: i64 = host
        .eval_server(r#"return store.one({"scopes", "tools", "edges", "ripgrep>grep"})"#)
        .unwrap();
    assert_eq!(rg_grep, 1);

    host.eval_server::<()>(
        r#"
        rt.start_flow(function()
          store.append({ type = "decay", scope = "tools" })
        end)
    "#,
    )
    .unwrap();
    // decay is a server-only append from the server VM (no hop). verify replay.
    host.eval_server::<()>("store.verify()").unwrap();
    let decayed: f64 = host
        .eval_server(r#"return store.one({"scopes", "tools", "edges", "ripgrep>grep"})"#)
        .unwrap();
    assert!((decayed - 0.9).abs() < 1e-9, "{decayed}");
}

#[test]
fn microblog_fanout_on_write() {
    let code = compile(include_str!("../hop/microblog.hop"));
    let host = Host::new(&["A", "B"], &code, false).expect("host");

    host.eval_server::<()>(
        r#"
        rt.start_flow(function()
          store.append({ type = "join", sid = "A", name = "Ada" })
          store.append({ type = "join", sid = "B", name = "Bob" })
          store.append({ type = "follow", sid = "B", who = "A" })
          store.append({ type = "post", sid = "A", text = "hello tape" })
        end)
    "#,
    )
    .unwrap();

    let count: i64 = host.eval_server(r#"return store.one({"post_count"})"#).unwrap();
    assert_eq!(count, 1);

    // A's own timeline and B's (follower) both got the post id (seq 3 —
    // join, join, follow, post).
    let a_n: i64 = host
        .eval_server(r#"local t = store.one({"timelines", "A"}); return #t"#)
        .unwrap();
    let b_n: i64 = host
        .eval_server(r#"local t = store.one({"timelines", "B"}); return #t"#)
        .unwrap();
    assert_eq!(a_n, 1, "author timeline");
    assert_eq!(b_n, 1, "follower timeline");
    let text: String = host
        .eval_server(r#"return store.one({"posts", 3, "text"})"#)
        .unwrap();
    assert_eq!(text, "hello tape");
    host.eval_server::<()>("store.verify()").unwrap();
}

#[test]
fn chat_nested_room_messages() {
    let code = compile(include_str!("../hop/chat.hop"));
    let host = Host::new(&["A", "B"], &code, false).expect("host");
    host.fire("A", "sim_demo").unwrap();
    host.pump().unwrap();

    host.eval_server::<()>(
        r#"
        rt.start_flow(function()
          store.append({ type = "say", sid = "B", room = "lobby", text = "hi ada" })
          store.append({ type = "say", sid = "A", room = "random", text = "side room" })
        end)
    "#,
    )
    .unwrap();

    let n: i64 = host
        .eval_server(r#"local m = store.one({"rooms", "lobby", "messages"}); return #m"#)
        .unwrap();
    assert_eq!(n, 2);
    let side: i64 = host
        .eval_server(r#"local m = store.one({"rooms", "random", "messages"}); return #m"#)
        .unwrap();
    assert_eq!(side, 1);
    host.eval_server::<()>("store.verify()").unwrap();
}
