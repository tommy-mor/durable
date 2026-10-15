//! A flow is replaced whole: re-birthing a key releases the old flow's
//! other half.
//!
//! A panel is one flow with two halves: her server half asleep in
//! store.wait, and her browser half parked on the hop that started it.
//! Re-birthing her key kills both halves of the old flow — a `cancel`
//! packet carries the death across, in both directions — and only the
//! flow's own tab may send one.

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
  flow "chat" panel(room);
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
fn rebirthing_the_same_key_replaces_instead_of_duplicating() {
    let mut host = Cluster::new(&["A"], APP, false).expect("cluster");
    host.fire_args("A", "open_room", vec![s("lobby")]);
    host.pump();
    let first = host.server_parked();
    host.fire_args("A", "open_room", vec![s("lobby")]);
    host.pump();
    assert_eq!(host.server_parked().len(), 1, "still one server half");
    assert_eq!(host.browser_parked("A").len(), 1, "still one browser half");
    assert_ne!(host.server_parked(), first, "the old flow died, a new one lives");
    // and the survivor still works: two openings plus one write wake
    host.fire_args("A", "say", vec![s("lobby"), s("hi")]);
    host.pump();
    let shows = host.log().iter().filter(|l| l.contains("[browser A] show lobby")).count();
    assert_eq!(shows, 3, "{}", host.log().join("\n"));
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
