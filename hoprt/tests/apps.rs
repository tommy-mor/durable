//! Simulated-cluster tests for the larger hop apps. These prove hopc +
//! the durable store can run ranking (Sum edges + decay), microblog
//! (fan-out-on-write timelines), chat (nested list under a map), the
//! tournament bracket, and the ember compaction rig without a browser.
//! Store reads go through the same natives the apps use; every scenario
//! ends with replay verification.

use std::path::PathBuf;

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

    let votes = host.store_get(arr(vec![s("scopes"), s("tools"), s("votes")])).unwrap();
    assert_eq!(votes, i(3));
    let rg_grep = host
        .store_get(arr(vec![s("scopes"), s("tools"), s("edges"), s("ripgrep>grep")]))
        .unwrap();
    assert_eq!(rg_grep, i(1));

    // decay is a server-only append (no hop). verify replay after.
    host.append(map(&[("type", s("decay")), ("scope", s("tools"))])).unwrap();
    host.verify().unwrap();
    let decayed = host
        .store_get(arr(vec![s("scopes"), s("tools"), s("edges"), s("ripgrep>grep")]))
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

    let count = host.store_get(arr(vec![s("post_count")])).unwrap();
    assert_eq!(count, i(1));

    // A's own timeline and B's (follower) both got the post id (seq 3 —
    // join, join, follow, post).
    let a_tl = host.store_get(arr(vec![s("timelines"), s("A")])).unwrap();
    let b_tl = host.store_get(arr(vec![s("timelines"), s("B")])).unwrap();
    assert_eq!(arr_len(&a_tl), 1, "author timeline");
    assert_eq!(arr_len(&b_tl), 1, "follower timeline");
    let text = host.store_get(arr(vec![s("posts"), i(3), s("text")])).unwrap();
    assert_eq!(text, s("hello tape"));
    host.verify().unwrap();
}

fn report(host: &mut Cluster, tid: i64, mid: &str, winner: &str) {
    host.append(map(&[
        ("type", s("report")),
        ("tid", i(tid)),
        ("mid", s(mid)),
        ("winner", s(winner)),
    ]))
    .unwrap();
}

fn tget(host: &mut Cluster, tid: i64, rest: Vec<Value>) -> Value {
    let mut path = vec![s("tournaments"), i(tid)];
    path.extend(rest);
    host.store_get(arr(path)).unwrap()
}

#[test]
fn tournament_single_elim_seeding_scores_and_reopen() {
    let mut host = Cluster::new(&["A", "B"], include_str!("../hop/tournament.hop"), false).unwrap();

    // create without a format: defaults to single elimination
    host.append(map(&[("type", s("create")), ("name", s("spring cup"))])).unwrap();
    for p in ["a", "b", "c", "d", "e"] {
        host.append(map(&[("type", s("enter")), ("tid", i(0)), ("player", s(p))])).unwrap();
    }
    // duplicate names are rejected; seeding is editable: promote e, drop d,
    // re-enter a … no wait, a is taken — enter "f" then drop it again
    host.append(map(&[("type", s("enter")), ("tid", i(0)), ("player", s("a"))])).unwrap();
    assert_eq!(arr_len(&tget(&mut host, 0, vec![s("players")])), 5, "duplicate rejected");
    host.append(map(&[("type", s("move")), ("tid", i(0)), ("i", i(4))])).unwrap();
    let players = tget(&mut host, 0, vec![s("players")]);
    assert_eq!(host.store_get(arr(vec![s("tournaments"), i(0), s("players"), i(3)])).unwrap(), s("e"));
    assert_eq!(arr_len(&players), 5);
    host.append(map(&[("type", s("drop")), ("tid", i(0)), ("i", i(4))])).unwrap();
    assert_eq!(arr_len(&tget(&mut host, 0, vec![s("players")])), 4, "d dropped");
    host.append(map(&[("type", s("enter")), ("tid", i(0)), ("player", s("d"))])).unwrap();

    // seeds now a b c e d. size 8, rounds 3; order 1 8 4 5 2 7 3 6 puts
    // byes on seeds 1, 2, 3 (a, b, c); r1m2 is e (seed 4) vs d (seed 5)
    host.append(map(&[("type", s("start")), ("tid", i(0))])).unwrap();
    assert_eq!(tget(&mut host, 0, vec![s("status")]), s("live"));
    assert_eq!(tget(&mut host, 0, vec![s("rounds")]), i(3));
    assert_eq!(tget(&mut host, 0, vec![s("matches"), s("r1m1"), s("winner")]), s("a"));
    assert_eq!(tget(&mut host, 0, vec![s("matches"), s("r1m2"), s("p1")]), s("e"));
    assert_eq!(tget(&mut host, 0, vec![s("matches"), s("r1m2"), s("p2")]), s("d"));
    assert_eq!(tget(&mut host, 0, vec![s("matches"), s("r2m2"), s("p1")]), s("b"));
    assert_eq!(tget(&mut host, 0, vec![s("matches"), s("r2m2"), s("p2")]), s("c"));

    // guards: late entry, foreign winner, double report — all no-ops
    host.append(map(&[("type", s("enter")), ("tid", i(0)), ("player", s("late"))])).unwrap();
    assert_eq!(arr_len(&tget(&mut host, 0, vec![s("players")])), 5, "signup locked");
    report(&mut host, 0, "r1m2", "nobody");
    assert_eq!(tget(&mut host, 0, vec![s("matches"), s("r1m2"), s("winner")]), Value::Nil);

    // a report can carry scores
    host.append(map(&[
        ("type", s("report")),
        ("tid", i(0)),
        ("mid", s("r1m2")),
        ("winner", s("d")),
        ("s1", i(1)),
        ("s2", i(2)),
    ]))
    .unwrap();
    assert_eq!(tget(&mut host, 0, vec![s("matches"), s("r1m2"), s("s2")]), i(2));
    report(&mut host, 0, "r1m2", "e");
    assert_eq!(
        tget(&mut host, 0, vec![s("matches"), s("r1m2"), s("winner")]),
        s("d"),
        "decided match cannot be re-reported"
    );

    report(&mut host, 0, "r2m1", "a");
    report(&mut host, 0, "r2m2", "c");
    report(&mut host, 0, "r3m1", "c");
    assert_eq!(tget(&mut host, 0, vec![s("champion")]), s("c"));
    assert_eq!(tget(&mut host, 0, vec![s("status")]), s("done"));

    // reopen the semifinal c won: the cascade pulls c back out of the
    // final and uncrowns them — the final's other side is untouched
    host.append(map(&[("type", s("unreport")), ("tid", i(0)), ("mid", s("r2m2"))])).unwrap();
    assert_eq!(tget(&mut host, 0, vec![s("champion")]), Value::Nil);
    assert_eq!(tget(&mut host, 0, vec![s("status")]), s("live"));
    assert_eq!(tget(&mut host, 0, vec![s("matches"), s("r3m1"), s("winner")]), Value::Nil);
    assert_eq!(tget(&mut host, 0, vec![s("matches"), s("r3m1"), s("p2")]), Value::Nil);
    assert_eq!(tget(&mut host, 0, vec![s("matches"), s("r3m1"), s("p1")]), s("a"));

    // history went the other way
    report(&mut host, 0, "r2m2", "b");
    report(&mut host, 0, "r3m1", "b");
    assert_eq!(tget(&mut host, 0, vec![s("champion")]), s("b"));

    // the lobby snapshot goes through store(["tournaments", store.keys])
    let snap = host.call_server("snapshot", vec![Value::Nil]).unwrap();
    let list = snap.get_field("list");
    assert_eq!(arr_len(&list), 1);
    let snap = host.call_server("snapshot", vec![i(0)]).unwrap();
    host.call_server("render", vec![snap]).unwrap();
    let log = host.log().join("\n");
    assert!(log.contains("champion: b"), "{log}");

    host.verify().unwrap();
}

#[test]
fn tournament_double_elim_losers_bracket_and_reset() {
    let mut host = Cluster::new(&["A"], include_str!("../hop/tournament.hop"), false).unwrap();
    host.append(map(&[("type", s("create")), ("name", s("de")), ("format", s("double"))]))
        .unwrap();
    for p in ["a", "b", "c", "d"] {
        host.append(map(&[("type", s("enter")), ("tid", i(0)), ("player", s(p))])).unwrap();
    }
    host.append(map(&[("type", s("start")), ("tid", i(0))])).unwrap();

    // WB: w1m1 = a vs d, w1m2 = b vs c (order 1 4 2 3). losers drop.
    report(&mut host, 0, "w1m1", "a");
    report(&mut host, 0, "w1m2", "b");
    assert_eq!(tget(&mut host, 0, vec![s("matches"), s("l1m1"), s("p1")]), s("d"));
    assert_eq!(tget(&mut host, 0, vec![s("matches"), s("l1m1"), s("p2")]), s("c"));

    report(&mut host, 0, "w2m1", "a"); // b drops to the LB final
    assert_eq!(tget(&mut host, 0, vec![s("matches"), s("l2m1"), s("p2")]), s("b"));
    report(&mut host, 0, "l1m1", "d");
    assert_eq!(tget(&mut host, 0, vec![s("matches"), s("l2m1"), s("p1")]), s("d"));
    report(&mut host, 0, "l2m1", "b");

    // grand final: a (undefeated) vs b (through the losers bracket).
    // b winning forces the bracket reset; the reset decides everything.
    assert_eq!(tget(&mut host, 0, vec![s("matches"), s("gf1"), s("p1")]), s("a"));
    assert_eq!(tget(&mut host, 0, vec![s("matches"), s("gf1"), s("p2")]), s("b"));
    report(&mut host, 0, "gf1", "b");
    assert_eq!(tget(&mut host, 0, vec![s("champion")]), Value::Nil, "reset pending");
    assert_eq!(tget(&mut host, 0, vec![s("matches"), s("gf2"), s("p1")]), s("a"));
    report(&mut host, 0, "gf2", "b");
    assert_eq!(tget(&mut host, 0, vec![s("champion")]), s("b"));

    // reopen the reset: uncrowned, replayable the other way
    host.append(map(&[("type", s("unreport")), ("tid", i(0)), ("mid", s("gf2"))])).unwrap();
    assert_eq!(tget(&mut host, 0, vec![s("status")]), s("live"));
    report(&mut host, 0, "gf2", "a");
    assert_eq!(tget(&mut host, 0, vec![s("champion")]), s("a"));

    host.verify().unwrap();
}

#[test]
fn tournament_round_robin_standings_decide() {
    let mut host = Cluster::new(&["A"], include_str!("../hop/tournament.hop"), false).unwrap();
    host.append(map(&[("type", s("create")), ("name", s("rr")), ("format", s("robin"))]))
        .unwrap();
    for p in ["a", "b", "c"] {
        host.append(map(&[("type", s("enter")), ("tid", i(0)), ("player", s(p))])).unwrap();
    }
    host.append(map(&[("type", s("start")), ("tid", i(0))])).unwrap();

    // circle method, 3 players padded with a bye: r1 = b vs c,
    // r2 = a vs c, r3 = a vs b — everyone plays everyone once
    assert_eq!(tget(&mut host, 0, vec![s("matches"), s("r1m1"), s("p1")]), s("b"));
    assert_eq!(tget(&mut host, 0, vec![s("matches"), s("r2m1"), s("p1")]), s("a"));
    assert_eq!(tget(&mut host, 0, vec![s("matches"), s("r3m1"), s("p2")]), s("b"));

    report(&mut host, 0, "r1m1", "b");
    report(&mut host, 0, "r2m1", "a");
    assert_eq!(tget(&mut host, 0, vec![s("status")]), s("live"), "one match left");
    report(&mut host, 0, "r3m1", "a");
    assert_eq!(tget(&mut host, 0, vec![s("champion")]), s("a"), "a is 2-0");
    assert_eq!(tget(&mut host, 0, vec![s("status")]), s("done"));

    // flip a result into a three-way tie: done, but nobody is crowned
    host.append(map(&[("type", s("unreport")), ("tid", i(0)), ("mid", s("r2m1"))])).unwrap();
    assert_eq!(tget(&mut host, 0, vec![s("status")]), s("live"));
    assert_eq!(tget(&mut host, 0, vec![s("champion")]), Value::Nil);
    report(&mut host, 0, "r2m1", "c");
    assert_eq!(tget(&mut host, 0, vec![s("status")]), s("done"));
    assert_eq!(tget(&mut host, 0, vec![s("champion")]), Value::Nil, "1-1-1 tie");

    // deleting a tournament removes it from the lobby select
    host.append(map(&[("type", s("delete")), ("tid", i(0))])).unwrap();
    let snap = host.call_server("snapshot", vec![Value::Nil]).unwrap();
    assert_eq!(arr_len(&snap.get_field("list")), 0);

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

    let lobby = host.store_get(arr(vec![s("rooms"), s("lobby"), s("messages")])).unwrap();
    assert_eq!(arr_len(&lobby), 2);
    let random = host.store_get(arr(vec![s("rooms"), s("random"), s("messages")])).unwrap();
    assert_eq!(arr_len(&random), 1);
    host.verify().unwrap();
}

fn ember_src() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../ember2/ember.hop");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

#[test]
fn ember_compaction_rig_projects_ranks_and_talks() {
    let mut host = Cluster::new(&["A"], &ember_src(), false).unwrap();
    host.fire("A", "sim_boot");
    host.pump();
    host.assert_quiescent();

    assert_eq!(host.store_get(arr(vec![s("memories")])).unwrap(), i(6));
    assert_eq!(
        host.store_get(arr(vec![s("declaration")])).unwrap(),
        s("keep what is alive")
    );
    let init = host.store_get(arr(vec![s("all"), s("i1")])).unwrap();
    assert_eq!(init.get_field("api_key"), Value::Nil, "init must not keep the key");
    assert_eq!(init.get_field("capacity"), i(10));

    host.set_dom("A", "#budget", "2");
    host.fire("A", "seal");
    host.pump();
    assert_eq!(
        host.store_get(arr(vec![s("memories")])).unwrap(),
        i(2),
        "seal keeps the budget"
    );
    assert!(
        !matches!(
            host.store_get(arr(vec![s("current"), s("p1")])).unwrap(),
            Value::Nil
        ),
        "p1 won both votes and should remain"
    );

    host.set_dom("A", "#draft", "hello ember");
    host.fire("A", "send");
    host.pump();
    host.assert_quiescent();
    assert_eq!(
        host.store_get(arr(vec![s("memories")])).unwrap(),
        i(4),
        "perception + response after talk"
    );
    let html = host.dom("A", "#app");
    assert!(html.contains("hello ember"), "perception painted: {html}");
    assert!(html.contains("newest first"), "{html}");
    assert!(html.contains("compact"), "{html}");

    host.verify().unwrap();
}
