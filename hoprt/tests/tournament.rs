//! Simulated-cluster tests for the tournament planner's Challonge-parity
//! features: swiss pairing and byes, two-stage groups, forfeits that
//! cascade through the link graph, check-in, rename propagation, the
//! third-place match, reset, shuffle, and the server-side admin gate.
//! Every scenario ends with replay verification against the tape.

use hoprt::harness::Cluster;
use hoprt::value::{coerce_str, Value};

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
    Value::map(entries.iter().map(|(k, v)| (Value::str(*k), v.clone())).collect())
}

fn t_host(format: &str, players: &[&str]) -> Cluster {
    let mut host =
        Cluster::new(&["A", "B"], include_str!("../hop/tournament.hop"), false).unwrap();
    host.append(map(&[("type", s("create")), ("name", s("t")), ("format", s(format))]))
        .unwrap();
    for p in players {
        host.append(map(&[("type", s("enter")), ("tid", i(0)), ("player", s(p))])).unwrap();
    }
    host
}

fn report(host: &mut Cluster, mid: &str, winner: &str) {
    host.append(map(&[
        ("type", s("report")),
        ("tid", i(0)),
        ("mid", s(mid)),
        ("winner", s(winner)),
    ]))
    .unwrap();
}

fn ev(host: &mut Cluster, kind: &str, extra: &[(&str, Value)]) {
    let mut fields = vec![("type", s(kind)), ("tid", i(0))];
    fields.extend_from_slice(extra);
    host.append(map(&fields)).unwrap();
}

fn tget(host: &mut Cluster, rest: Vec<Value>) -> Value {
    let mut path = vec![s("tournaments"), i(0)];
    path.extend(rest);
    host.store_get(arr(path)).unwrap()
}

fn mget(host: &mut Cluster, mid: &str, field: &str) -> Value {
    tget(host, vec![s("matches"), s(mid), s(field)])
}

/// A string field, or "" when nil (coerce_str would print "nil").
fn sfield(v: &Value, name: &str) -> String {
    match v.get_field(name) {
        Value::Str(x) => x.to_string(),
        _ => String::new(),
    }
}

/// (mid, p1, p2, winner-or-"") for every match.
fn matches_of(host: &mut Cluster) -> Vec<(String, String, String, String)> {
    let v = tget(host, vec![s("matches")]);
    let Value::Map(m) = v else { return Vec::new() };
    let m = m.borrow();
    m.iter()
        .map(|(k, v)| (coerce_str(k), sfield(v, "p1"), sfield(v, "p2"), sfield(v, "winner")))
        .collect()
}

/// Report every open match, better-ranked player wins, until none remain.
fn play_out(host: &mut Cluster, ranking: &[&str]) {
    let rank = |p: &str| ranking.iter().position(|r| *r == p).unwrap_or(usize::MAX);
    loop {
        let open: Vec<(String, String, String)> = matches_of(host)
            .into_iter()
            .filter(|(_, p1, p2, w)| {
                w.is_empty() && !p1.is_empty() && !p2.is_empty() && p1 != "__bye" && p2 != "__bye"
            })
            .map(|(mid, p1, p2, _)| (mid, p1, p2))
            .collect();
        if open.is_empty() {
            return;
        }
        for (mid, p1, p2) in open {
            let w = if rank(&p1) <= rank(&p2) { p1 } else { p2 };
            report(host, &mid, &w);
        }
    }
}

fn roster_names(host: &mut Cluster) -> Vec<String> {
    let rows = host.call_server("roster", vec![i(0)]).unwrap();
    let Value::Array(a) = rows else { panic!("roster returns an array") };
    let a = a.borrow();
    a.iter().map(|r| coerce_str(&r.get_field("name"))).collect()
}

// ── swiss ───────────────────────────────────────────────────────────────

#[test]
fn swiss_pairs_by_standings_rotates_byes_and_locks_history() {
    let mut host = t_host("swiss", &["a", "b", "c", "d", "e"]);
    ev(&mut host, "start", &[]);

    // 5 players: 3 rounds (ceil log2), round 1 is top half vs bottom
    // half by seed, the lowest seed takes the bye
    assert_eq!(tget(&mut host, vec![s("rounds")]), i(3));
    assert_eq!(tget(&mut host, vec![s("round")]), i(1));
    assert_eq!(mget(&mut host, "r1m1", "p1"), s("a"));
    assert_eq!(mget(&mut host, "r1m1", "p2"), s("c"));
    assert_eq!(mget(&mut host, "r1m2", "p1"), s("b"));
    assert_eq!(mget(&mut host, "r1m2", "p2"), s("d"));
    assert_eq!(mget(&mut host, "r1m3", "p2"), s("__bye"));
    assert_eq!(mget(&mut host, "r1m3", "winner"), s("e"), "a bye is a free win");

    // pairing next round needs every result in
    ev(&mut host, "next_round", &[]);
    assert_eq!(tget(&mut host, vec![s("round")]), i(1), "round incomplete — no pairing");

    report(&mut host, "r1m1", "a");
    report(&mut host, "r1m2", "b");
    ev(&mut host, "next_round", &[]);
    assert_eq!(tget(&mut host, vec![s("round")]), i(2));

    // 1-pointers pair off (a-b, e-c); d, lowest without one, gets the bye
    assert_eq!(mget(&mut host, "r2m1", "p1"), s("a"));
    assert_eq!(mget(&mut host, "r2m1", "p2"), s("b"));
    assert_eq!(mget(&mut host, "r2m2", "p1"), s("e"));
    assert_eq!(mget(&mut host, "r2m2", "p2"), s("c"));
    assert_eq!(mget(&mut host, "r2m3", "p1"), s("d"));
    assert_eq!(mget(&mut host, "r2m3", "winner"), s("d"));

    report(&mut host, "r2m1", "a");
    report(&mut host, "r2m2", "e");
    ev(&mut host, "next_round", &[]);
    assert_eq!(tget(&mut host, vec![s("round")]), i(3));

    // leaders meet; b-d is a forced rematch (only pair left); the bye
    // rotates to c — e, d, c each had exactly one
    assert_eq!(mget(&mut host, "r3m1", "p1"), s("a"));
    assert_eq!(mget(&mut host, "r3m1", "p2"), s("e"));
    assert_eq!(mget(&mut host, "r3m2", "p1"), s("b"));
    assert_eq!(mget(&mut host, "r3m2", "p2"), s("d"));
    assert_eq!(mget(&mut host, "r3m3", "p1"), s("c"));
    assert_eq!(mget(&mut host, "r3m3", "winner"), s("c"));

    // history is locked: only the round being played reopens
    ev(&mut host, "unreport", &[("mid", s("r1m1"))]);
    assert_eq!(mget(&mut host, "r1m1", "winner"), s("a"));

    report(&mut host, "r3m1", "a");
    report(&mut host, "r3m2", "d");
    assert_eq!(tget(&mut host, vec![s("status")]), s("review"), "all rounds in");
    ev(&mut host, "finalize", &[]);
    assert_eq!(tget(&mut host, vec![s("champion")]), s("a"), "3-0");
    assert_eq!(tget(&mut host, vec![s("status")]), s("done"));

    host.verify().unwrap();
}

// ── two-stage groups ────────────────────────────────────────────────────

#[test]
fn groups_snake_seed_then_finals_from_standings() {
    let mut host = t_host("groups", &["a", "b", "c", "d", "e", "f", "g", "h"]);
    ev(&mut host, "start", &[]);
    assert_eq!(tget(&mut host, vec![s("stage")]), s("groups"));

    // snake seeding, two pools of four: 1 4 5 8 / 2 3 6 7
    let ms = matches_of(&mut host);
    let pool = |grp: &str| -> Vec<String> {
        let mut names: Vec<String> = ms
            .iter()
            .filter(|(mid, ..)| mid.starts_with(grp))
            .flat_map(|(_, p1, p2, _)| [p1.clone(), p2.clone()])
            .collect();
        names.sort();
        names.dedup();
        names
    };
    assert_eq!(pool("g1r"), vec!["a", "d", "e", "h"]);
    assert_eq!(pool("g2r"), vec!["b", "c", "f", "g"]);

    // finals refuse to start while pools are open
    ev(&mut host, "start_finals", &[]);
    assert_eq!(tget(&mut host, vec![s("stage")]), s("groups"));

    // play the pools to a clean hierarchy: a > d > e > h, b > c > f > g
    play_out(&mut host, &["a", "b", "c", "d", "e", "f", "g", "h"]);
    ev(&mut host, "start_finals", &[]);
    assert_eq!(tget(&mut host, vec![s("stage")]), s("finals"));

    // winners seeded first (a, b), runners-up after (d, c):
    // semis are 1v4 and 2v3
    assert_eq!(mget(&mut host, "r1m1", "p1"), s("a"));
    assert_eq!(mget(&mut host, "r1m1", "p2"), s("c"));
    assert_eq!(mget(&mut host, "r1m2", "p1"), s("b"));
    assert_eq!(mget(&mut host, "r1m2", "p2"), s("d"));

    // the sealed pool stage refuses to reopen
    ev(&mut host, "unreport", &[("mid", s("g1r1m1"))]);
    assert_ne!(mget(&mut host, "g1r1m1", "winner"), Value::Nil);

    report(&mut host, "r1m1", "a");
    report(&mut host, "r1m2", "b");
    report(&mut host, "r2m1", "a");
    assert_eq!(tget(&mut host, vec![s("champion")]), s("a"));
    assert_eq!(tget(&mut host, vec![s("status")]), s("done"));

    host.verify().unwrap();
}

// ── forfeits ────────────────────────────────────────────────────────────

#[test]
fn forfeit_decides_open_matches_and_future_arrivals() {
    // single elim, 4 players: r1m1 = a vs d, r1m2 = b vs c
    let mut host = t_host("single", &["a", "b", "c", "d"]);
    ev(&mut host, "start", &[]);
    report(&mut host, "r1m1", "a");

    // a won a semifinal, then forfeits while waiting in the final. When
    // b arrives there, the match auto-decides against the dropped player.
    ev(&mut host, "forfeit", &[("name", s("a"))]);
    report(&mut host, "r1m2", "b");
    assert_eq!(mget(&mut host, "r2m1", "winner"), s("b"));
    assert_eq!(mget(&mut host, "r2m1", "ff"), Value::Bool(true));
    assert_eq!(tget(&mut host, vec![s("champion")]), s("b"));

    // forfeit results are structural — no reopen
    ev(&mut host, "unreport", &[("mid", s("r2m1"))]);
    assert_eq!(mget(&mut host, "r2m1", "winner"), s("b"));

    host.verify().unwrap();
}

#[test]
fn forfeit_cascades_through_the_losers_bracket() {
    // double elim, 4 players: w1m1 = a vs d, w1m2 = b vs c
    let mut host = t_host("double", &["a", "b", "c", "d"]);
    ev(&mut host, "start", &[]);
    report(&mut host, "w1m1", "a");
    report(&mut host, "w1m2", "b");

    // d and c meet in l1m1; d forfeits — c advances by forfeit
    ev(&mut host, "forfeit", &[("name", s("d"))]);
    assert_eq!(mget(&mut host, "l1m1", "winner"), s("c"));
    assert_eq!(mget(&mut host, "l1m1", "ff"), Value::Bool(true));
    assert_eq!(mget(&mut host, "l2m1", "p1"), s("c"));

    report(&mut host, "w2m1", "b"); // a drops to the LB final against c
    report(&mut host, "l2m1", "a");
    report(&mut host, "gf1", "b");
    assert_eq!(tget(&mut host, vec![s("champion")]), s("b"), "WB champ stays undefeated");

    host.verify().unwrap();
}

#[test]
fn forfeit_in_the_grand_final_crowns_without_a_reset() {
    let mut host = t_host("double", &["a", "b", "c", "d"]);
    ev(&mut host, "start", &[]);
    report(&mut host, "w1m1", "a");
    report(&mut host, "w1m2", "b");
    report(&mut host, "w2m1", "a");
    report(&mut host, "l1m1", "d");
    report(&mut host, "l2m1", "b");
    // gf1 = a (undefeated) vs b. a forfeits: no bracket reset against a
    // forfeit — b takes the title directly.
    ev(&mut host, "forfeit", &[("name", s("a"))]);
    assert_eq!(mget(&mut host, "gf1", "winner"), s("b"));
    assert_eq!(tget(&mut host, vec![s("matches"), s("gf2"), s("round")]), Value::Nil);
    assert_eq!(tget(&mut host, vec![s("champion")]), s("b"));

    host.verify().unwrap();
}

// ── check-in ────────────────────────────────────────────────────────────

#[test]
fn checkin_filters_the_field_and_marks_no_shows() {
    let mut host = t_host("single", &["a", "b", "c", "d"]);
    ev(&mut host, "checkin_open", &[]);
    assert_eq!(tget(&mut host, vec![s("status")]), s("checkin"));

    // checking in is only meaningful during the phase
    ev(&mut host, "checkin", &[("i", i(0)), ("v", Value::Bool(true))]);
    ev(&mut host, "checkin", &[("i", i(2)), ("v", Value::Bool(true))]);
    ev(&mut host, "start", &[]);
    assert_eq!(tget(&mut host, vec![s("status")]), s("live"));

    // only a and c play; b and d are marked out (no-shows)
    assert_eq!(mget(&mut host, "r1m1", "p1"), s("a"));
    assert_eq!(mget(&mut host, "r1m1", "p2"), s("c"));
    assert_eq!(tget(&mut host, vec![s("players"), s("b"), s("st")]), s("out"));
    assert_eq!(tget(&mut host, vec![s("players"), s("d"), s("st")]), s("out"));

    report(&mut host, "r1m1", "c");
    assert_eq!(tget(&mut host, vec![s("champion")]), s("c"));

    // reset clears the no-show marks and returns to signup
    ev(&mut host, "reset", &[]);
    assert_eq!(tget(&mut host, vec![s("status")]), s("signup"));
    assert_eq!(tget(&mut host, vec![s("champion")]), Value::Nil);
    assert!(matches_of(&mut host).is_empty(), "matches wiped");
    assert_eq!(tget(&mut host, vec![s("players"), s("b"), s("st")]), s("active"));
    assert_eq!(roster_names(&mut host).len(), 4, "entrants survive");

    host.verify().unwrap();
}

// ── rename ──────────────────────────────────────────────────────────────

#[test]
fn rename_chases_the_old_name_through_a_live_bracket() {
    let mut host = t_host("single", &["a", "b", "c", "d"]);
    ev(&mut host, "start", &[]);
    report(&mut host, "r1m1", "a");
    report(&mut host, "r1m2", "b");
    report(&mut host, "r2m1", "a");
    assert_eq!(tget(&mut host, vec![s("champion")]), s("a"));

    // a → alice: seat, match slots, winners, and the crown all follow
    ev(&mut host, "rename", &[("i", i(0)), ("name", s("alice"))]);
    assert_eq!(roster_names(&mut host)[0], "alice");
    assert_eq!(mget(&mut host, "r1m1", "p1"), s("alice"));
    assert_eq!(mget(&mut host, "r1m1", "winner"), s("alice"));
    assert_eq!(mget(&mut host, "r2m1", "winner"), s("alice"));
    assert_eq!(tget(&mut host, vec![s("champion")]), s("alice"));

    // a taken name is rejected
    ev(&mut host, "rename", &[("i", i(1)), ("name", s("alice"))]);
    assert_eq!(roster_names(&mut host)[1], "b");

    host.verify().unwrap();
}

// ── third place ─────────────────────────────────────────────────────────

#[test]
fn third_place_match_settles_bronze_and_placements() {
    let mut host = t_host("single", &["a", "b", "c", "d"]);
    ev(&mut host, "config", &[("k", s("third")), ("v", Value::Bool(true))]);
    ev(&mut host, "start", &[]);

    // semifinal losers drop into "3p"
    report(&mut host, "r1m1", "a"); // d out
    report(&mut host, "r1m2", "b"); // c out
    assert_eq!(mget(&mut host, "3p", "p1"), s("d"));
    assert_eq!(mget(&mut host, "3p", "p2"), s("c"));

    report(&mut host, "r2m1", "a");
    assert_eq!(tget(&mut host, vec![s("champion")]), s("a"));
    // the bronze match can still be played after the crown
    report(&mut host, "3p", "d");
    assert_eq!(mget(&mut host, "3p", "winner"), s("d"));

    let t = host.call_server("load_t", vec![i(0)]).unwrap();
    let places = host.call_server("placements", vec![t]).unwrap();
    let Value::Array(rows) = places else { panic!("placements is an array") };
    let rows = rows.borrow();
    let got: Vec<(String, i64)> = rows
        .iter()
        .map(|r| {
            (coerce_str(&r.get_field("name")), match r.get_field("place") {
                Value::Int(p) => p,
                other => panic!("place: {other}"),
            })
        })
        .collect();
    assert_eq!(
        got,
        vec![
            ("a".to_string(), 1),
            ("b".to_string(), 2),
            ("d".to_string(), 3),
            ("c".to_string(), 4)
        ]
    );

    host.verify().unwrap();
}

// ── seeding tools ───────────────────────────────────────────────────────

#[test]
fn bulk_entry_shuffle_and_reorder_replay() {
    let mut host = t_host("single", &[]);
    // one event, many names — duplicates (within and against) ignored
    host.append(map(&[
        ("type", s("enter_bulk")),
        ("tid", i(0)),
        ("players", arr(vec![s("a"), s("b"), s("c"), s("b"), s("d")])),
    ]))
    .unwrap();
    assert_eq!(roster_names(&mut host), vec!["a", "b", "c", "d"]);

    // the shuffle flow rolls rand() (a platform effect, deterministic in
    // the harness) and appends the permutation — replay carries it
    host.connect("A");
    host.pump();
    host.fire_args("A", "shuffle_t", vec![i(0)]);
    host.pump();
    let mut shuffled = roster_names(&mut host);
    assert_eq!(shuffled.len(), 4);
    shuffled.sort();
    assert_eq!(shuffled, vec!["a", "b", "c", "d"], "a permutation, not a mutation");

    host.verify().unwrap();
}

// ── the admin gate ──────────────────────────────────────────────────────

#[test]
fn mutating_flows_check_the_admin_bit_server_side() {
    let mut host =
        Cluster::new(&["A", "B"], include_str!("../hop/tournament.hop"), false).unwrap();
    // seed the tape before connecting, so the tournament is tid 0
    // (on_connect appends seen/view events of its own)
    host.append(map(&[("type", s("create")), ("name", s("t")), ("format", s("single"))]))
        .unwrap();
    for p in ["a", "b"] {
        host.append(map(&[("type", s("enter")), ("tid", i(0)), ("player", s(p))])).unwrap();
    }
    host.set_profile("B", "bob", false); // spectator
    host.connect("A");
    host.connect("B");
    host.pump();

    // the spectator's start bounces at the server, the admin's lands
    host.fire_args("B", "start_t", vec![i(0)]);
    host.pump();
    assert_eq!(tget(&mut host, vec![s("status")]), s("signup"), "gate held");
    host.fire_args("A", "start_t", vec![i(0)]);
    host.pump();
    assert_eq!(tget(&mut host, vec![s("status")]), s("live"));

    // spectator rendering: same live data, none of the controls
    let snap = host.call_server("snapshot", vec![i(0)]).unwrap();
    let spectator =
        format!("{}", host.call_server("app_view", vec![snap.clone(), Value::Bool(false), s("bob")]).unwrap());
    let admin =
        format!("{}", host.call_server("app_view", vec![snap, Value::Bool(true), s("ada")]).unwrap());
    assert!(!spectator.contains("forfeit"), "{spectator}");
    assert!(!spectator.contains("reset to signup"));
    assert!(admin.contains("reset to signup"));
    assert!(spectator.contains("signed in as bob"));

    host.verify().unwrap();
}
