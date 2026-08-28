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
fn tournament_bracket_lifecycle() {
    let code = compile(include_str!("../hop/tournament.hop"));
    let host = Host::new(&["A", "B"], &code, false).expect("host");

    // create (seq 0 = tid), five entrants, start. size 8, rounds 3;
    // standard order 1 8 4 5 2 7 3 6 puts byes on seeds 1, 2, 3.
    host.eval_server::<()>(
        r#"
        rt.start_flow(function()
          store.append({ type = "create", name = "spring cup" })
          store.append({ type = "enter", tid = 0, player = "a" })
          store.append({ type = "enter", tid = 0, player = "b" })
          store.append({ type = "enter", tid = 0, player = "c" })
          store.append({ type = "enter", tid = 0, player = "d" })
          store.append({ type = "enter", tid = 0, player = "e" })
          store.append({ type = "start", tid = 0 })
        end)
    "#,
    )
    .unwrap();

    let status: String = host
        .eval_server(r#"return store.one({"tournaments", 0, "status"})"#)
        .unwrap();
    assert_eq!(status, "live");
    let rounds: i64 = host
        .eval_server(r#"return store.one({"tournaments", 0, "rounds"})"#)
        .unwrap();
    assert_eq!(rounds, 3);

    // byes resolved at start: a (seed 1) already through to r2m1 as p1;
    // b and c (seeds 2, 3) feed BOTH sides of r2m2 — playable immediately
    let bye_winner: String = host
        .eval_server(r#"return store.one({"tournaments", 0, "matches", "r1m1", "winner"})"#)
        .unwrap();
    assert_eq!(bye_winner, "a");
    let r2m2: (String, String) = host
        .eval_server(
            r#"return store.one({"tournaments", 0, "matches", "r2m2", "p1"}),
                      store.one({"tournaments", 0, "matches", "r2m2", "p2"})"#,
        )
        .unwrap();
    assert_eq!(r2m2, ("b".into(), "c".into()));

    // guards: entering after start is a no-op; reporting a decided match
    // is a no-op; a winner not in the match is a no-op
    host.eval_server::<()>(
        r#"
        rt.start_flow(function()
          store.append({ type = "enter", tid = 0, player = "late" })
          store.append({ type = "report", tid = 0, match = "r1m1", winner = "e" })
          store.append({ type = "report", tid = 0, match = "r1m2", winner = "nobody" })
        end)
    "#,
    )
    .unwrap();
    let n: i64 = host
        .eval_server(r#"return count(store.one({"tournaments", 0, "players"}))"#)
        .unwrap();
    assert_eq!(n, 5, "signup is locked");
    let still_a: String = host
        .eval_server(r#"return store.one({"tournaments", 0, "matches", "r1m1", "winner"})"#)
        .unwrap();
    assert_eq!(still_a, "a", "decided match cannot be re-reported");

    // play it out: e beats d, a beats e, c beats b, c beats a
    host.eval_server::<()>(
        r#"
        rt.start_flow(function()
          store.append({ type = "report", tid = 0, match = "r1m2", winner = "e" })
          store.append({ type = "report", tid = 0, match = "r2m1", winner = "a" })
          store.append({ type = "report", tid = 0, match = "r2m2", winner = "c" })
          store.append({ type = "report", tid = 0, match = "r3m1", winner = "c" })
        end)
    "#,
    )
    .unwrap();

    let champion: String = host
        .eval_server(r#"return store.one({"tournaments", 0, "champion"})"#)
        .unwrap();
    assert_eq!(champion, "c");
    let status: String = host
        .eval_server(r#"return store.one({"tournaments", 0, "status"})"#)
        .unwrap();
    assert_eq!(status, "done");

    // the view renders from the same snapshot the browsers would get
    host.eval_server::<()>("render(snapshot(0))").unwrap();
    let log = host.log().join("\n");
    assert!(log.contains("champion: c"), "{log}");

    // the whole bracket — seeding, byes, advancement — replays from the tape
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
