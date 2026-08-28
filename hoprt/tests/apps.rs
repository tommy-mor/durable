//! Simulated-cluster tests for the larger hop apps. These prove hopc +
//! the durable store can run ranking (Sum edges + decay), microblog
//! (fan-out-on-write timelines), chat (nested list under a map), and the
//! tournament bracket without a browser. Store reads go through the same
//! natives the apps use; every scenario ends with replay verification.

use hoprt::harness::Cluster;
use hoprt::value::Value;

fn s(x: &str) -> Value {
    Value::str(x)
}

fn i(x: i64) -> Value {
    Value::Int(x)
}

fn arr(items: Vec<Value>) -> Value {
    Value::array(items)
}

fn map(entries: &[(&str, Value)]) -> Value {
    Value::map(
        entries
            .iter()
            .map(|(k, v)| (Value::str(*k), v.clone()))
            .collect(),
    )
}

fn arr_len(v: &Value) -> usize {
    match v {
        Value::Array(a) => a.borrow().len(),
        Value::Nil => 0,
        other => panic!("expected array, got {other}"),
    }
}

#[test]
fn ranking_votes_and_decay() {
    let mut host = Cluster::new(&["A", "B"], include_str!("../hop/ranking.hop"), false).unwrap();
    host.fire("A", "sim_demo");
    host.pump();
    host.assert_quiescent();

    let votes = host.store_one(arr(vec![s("scopes"), s("tools"), s("votes")])).unwrap();
    assert_eq!(votes, i(3));
    let rg_grep = host
        .store_one(arr(vec![s("scopes"), s("tools"), s("edges"), s("ripgrep>grep")]))
        .unwrap();
    assert_eq!(rg_grep, i(1));

    // decay is a server-only append (no hop). verify replay after.
    host.append(map(&[("type", s("decay")), ("scope", s("tools"))])).unwrap();
    host.verify().unwrap();
    let decayed = host
        .store_one(arr(vec![s("scopes"), s("tools"), s("edges"), s("ripgrep>grep")]))
        .unwrap();
    match decayed {
        Value::Float(f) => assert!((f - 0.9).abs() < 1e-9, "{f}"),
        other => panic!("expected float edge after decay, got {other}"),
    }
}

#[test]
fn microblog_fans_out_on_write() {
    let mut host = Cluster::new(&["A", "B"], include_str!("../hop/microblog.hop"), false).unwrap();

    host.append(map(&[("type", s("join")), ("sid", s("A")), ("name", s("Ada"))])).unwrap();
    host.append(map(&[("type", s("join")), ("sid", s("B")), ("name", s("Bob"))])).unwrap();
    host.append(map(&[("type", s("follow")), ("sid", s("B")), ("who", s("A"))])).unwrap();
    host.append(map(&[("type", s("post")), ("sid", s("A")), ("text", s("hello tape"))])).unwrap();

    let count = host.store_one(arr(vec![s("post_count")])).unwrap();
    assert_eq!(count, i(1));

    // A's own timeline and B's (follower) both got the post id (seq 3 —
    // join, join, follow, post).
    let a_tl = host.store_one(arr(vec![s("timelines"), s("A")])).unwrap();
    let b_tl = host.store_one(arr(vec![s("timelines"), s("B")])).unwrap();
    assert_eq!(arr_len(&a_tl), 1, "author timeline");
    assert_eq!(arr_len(&b_tl), 1, "follower timeline");
    let text = host.store_one(arr(vec![s("posts"), i(3), s("text")])).unwrap();
    assert_eq!(text, s("hello tape"));
    host.verify().unwrap();
}

#[test]
fn tournament_bracket_lifecycle() {
    let mut host = Cluster::new(&["A", "B"], include_str!("../hop/tournament.hop"), false).unwrap();

    // create (seq 0 = tid), five entrants, start. size 8, rounds 3;
    // standard order 1 8 4 5 2 7 3 6 puts byes on seeds 1, 2, 3.
    host.append(map(&[("type", s("create")), ("name", s("spring cup"))])).unwrap();
    for p in ["a", "b", "c", "d", "e"] {
        host.append(map(&[("type", s("enter")), ("tid", i(0)), ("player", s(p))])).unwrap();
    }
    host.append(map(&[("type", s("start")), ("tid", i(0))])).unwrap();

    let t = |host: &mut Cluster, rest: Vec<Value>| {
        let mut path = vec![s("tournaments"), i(0)];
        path.extend(rest);
        host.store_one(arr(path)).unwrap()
    };

    assert_eq!(t(&mut host, vec![s("status")]), s("live"));
    assert_eq!(t(&mut host, vec![s("rounds")]), i(3));

    // byes resolved at start: a (seed 1) already through to r2m1 as p1;
    // b and c (seeds 2, 3) feed BOTH sides of r2m2 — playable immediately
    assert_eq!(t(&mut host, vec![s("matches"), s("r1m1"), s("winner")]), s("a"));
    assert_eq!(t(&mut host, vec![s("matches"), s("r2m2"), s("p1")]), s("b"));
    assert_eq!(t(&mut host, vec![s("matches"), s("r2m2"), s("p2")]), s("c"));

    // guards: entering after start is a no-op; reporting a decided match
    // is a no-op; a winner not in the match is a no-op
    host.append(map(&[("type", s("enter")), ("tid", i(0)), ("player", s("late"))])).unwrap();
    host.append(map(&[
        ("type", s("report")),
        ("tid", i(0)),
        ("match", s("r1m1")),
        ("winner", s("d")),
    ]))
    .unwrap();
    let players = t(&mut host, vec![s("players")]);
    assert_eq!(arr_len(&players), 5, "signup is locked");
    assert_eq!(
        t(&mut host, vec![s("matches"), s("r1m1"), s("winner")]),
        s("a"),
        "decided match cannot be re-reported"
    );

    // play it out: e beats d, a beats e, c beats b, c beats a
    for (mid, w) in [("r1m2", "e"), ("r2m1", "a"), ("r2m2", "c"), ("r3m1", "c")] {
        host.append(map(&[
            ("type", s("report")),
            ("tid", i(0)),
            ("match", s(mid)),
            ("winner", s(w)),
        ]))
        .unwrap();
    }

    assert_eq!(t(&mut host, vec![s("champion")]), s("c"));
    assert_eq!(t(&mut host, vec![s("status")]), s("done"));

    // the view renders from the same snapshot the browsers would get
    let snap = host.call_server("snapshot", vec![i(0)]).unwrap();
    host.call_server("render", vec![snap]).unwrap();
    let log = host.log().join("\n");
    assert!(log.contains("champion: c"), "{log}");

    // the whole bracket — seeding, byes, advancement — replays from the tape
    host.verify().unwrap();
}

#[test]
fn chat_rooms_are_isolated_lists() {
    let mut host = Cluster::new(&["A", "B"], include_str!("../hop/chat.hop"), false).unwrap();
    host.fire("A", "sim_demo");
    host.pump();

    host.append(map(&[
        ("type", s("say")),
        ("sid", s("B")),
        ("room", s("lobby")),
        ("text", s("hi ada")),
    ]))
    .unwrap();
    host.append(map(&[
        ("type", s("say")),
        ("sid", s("B")),
        ("room", s("random")),
        ("text", s("psst")),
    ]))
    .unwrap();

    let lobby = host.store_one(arr(vec![s("rooms"), s("lobby"), s("messages")])).unwrap();
    assert_eq!(arr_len(&lobby), 2);
    let random = host.store_one(arr(vec![s("rooms"), s("random"), s("messages")])).unwrap();
    assert_eq!(arr_len(&random), 1);
    host.verify().unwrap();
}
