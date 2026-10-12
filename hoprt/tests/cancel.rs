//! A flow is cancelled whole: killing one half releases the other.
//!
//! A panel is one flow with two halves: her server half asleep in
//! store.wait, and her browser half parked on the hop that started it.
//! Killing only the server half (cancel_origin on a room switch) used to
//! strand the browser half forever — one dead frame per switch, per tab,
//! for the life of the tab. A `cancel` packet now carries the death across,
//! in both directions, and only the flow's own tab may send one.

use hoprt::harness::Cluster;
use hoprt::value::Value;

fn s(x: &str) -> Value {
    Value::str(x)
}

const APP: &str = r#"
schema {
  rooms: map<record { messages: list<leaf> }>
}

fn reduce(event) {
  store(["rooms", event.room, "messages", store.push(event.text)]);
}

fn panel(room) {
  server!();
  while true {
    let ms = store(["rooms", room, "messages"]);
    browser!();
    print("show " .. room);
    server!();
    store.wait(["rooms", room, "messages"]);
  }
}

fn open_room(room) {
  server!();
  cancel_origin();
  browser!();
  spawn panel(room);
}

fn open_then_close() {
  let h = spawn panel("lobby");
  close_later(h);
}

fn close_later(h) {
  cancel(h);
}

fn say(room, text) {
  server!();
  store.append({ type = "say", room = room, text = text });
}
"#;

#[test]
fn switching_rooms_leaves_no_dead_halves() {
    let mut host = Cluster::new(&["A"], APP, false).expect("cluster");
    for room in ["lobby", "dev", "x", "lobby"] {
        host.fire_args("A", "open_room", vec![s(room)]);
        host.pump();
        assert_eq!(host.server_parked().len(), 1, "one live panel on the server after {room}");
        assert_eq!(
            host.browser_parked("A").len(),
            1,
            "only the live panel's browser half is parked after {room}: {:?}",
            host.browser_parked("A")
        );
        assert_eq!(host.server_parked(), host.browser_parked("A"), "the two halves are one flow");
    }
    // and the survivor still works
    host.fire_args("A", "say", vec![s("lobby"), s("hi")]);
    host.pump();
    let shows = host.log().iter().filter(|l| l.contains("[browser A] show lobby")).count();
    assert_eq!(shows, 3, "lobby shown twice on opening, once on the write");
}

#[test]
fn a_tab_cancelling_her_own_panel_kills_both_halves() {
    let mut host = Cluster::new(&["A"], APP, false).expect("cluster");
    host.fire("A", "open_then_close");
    host.pump();
    assert!(host.server_parked().is_empty(), "server half died: {:?}", host.server_parked());
    assert!(host.browser_parked("A").is_empty(), "browser half died");
    // nothing wakes on a later write
    host.fire_args("A", "say", vec![s("lobby"), s("hi")]);
    host.pump();
    host.assert_quiescent();
}

#[test]
fn a_tab_cannot_cancel_another_tabs_panel() {
    let mut host = Cluster::new(&["A", "B"], APP, false).expect("cluster");
    host.fire_args("A", "open_room", vec![s("lobby")]);
    host.pump();
    let flow = host.server_parked()[0].clone();
    host.inject(
        "B",
        Value::map([(s("kind"), s("cancel")), (s("flow"), s(&flow)), (s("to"), s("server"))].into_iter().collect()),
    );
    host.pump();
    let log = host.log().join("\n");
    assert!(log.contains(&format!("cancel for flow {flow} from B dropped: not hers")), "{log}");
    assert_eq!(host.server_parked(), vec![flow.clone()], "A's panel lives");
    assert_eq!(host.browser_parked("A"), vec![flow], "and her browser half too");
}

#[test]
fn closing_a_tab_sends_nothing_to_it() {
    let mut host = Cluster::new(&["A", "B"], APP, false).expect("cluster");
    host.fire_args("B", "open_room", vec![s("lobby")]);
    host.pump();
    let before = host.log().len();
    host.disconnect("B");
    host.pump();
    assert!(host.server_parked().is_empty());
    let after: Vec<String> = host.log()[before..].to_vec();
    assert!(
        !after.iter().any(|l| l.contains(" cancel ")),
        "a closed tab gets no cancels: {after:?}"
    );
}
