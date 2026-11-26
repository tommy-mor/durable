//! Only the party a parked flow awaits may answer it.
//!
//! Flow ids are guessable ("A#2"), and before `from` stamping a reply or
//! error packet from any socket popped whichever flow it named: one tab
//! could resume another tab's server flow with a value of her choosing,
//! including a panel asleep in store.wait. The transport now stamps the
//! sender on every packet and the server drops answers from anyone but
//! the awaited party.

use hoprt::harness::Cluster;
use hoprt::value::Value;

fn s(x: &str) -> Value {
    Value::str(x)
}

fn pkt(entries: &[(&str, Value)]) -> Value {
    Value::map(entries.iter().map(|(k, v)| (s(k), v.clone())).collect())
}

const APP: &str = r#"
schema { n: sum }

fn reduce(e) {
  store(["n", store.add(1)]);
}

fn panel() {
  server!();
  while true {
    let v = store(["n"]);
    browser!();
    print("n=" .. v);
    server!();
    store.wait(["n"]);
  }
}

fn slow() {
  server!();
  let r = bash("echo real");
  print("bash said " .. r.stdout);
}

fn bump() {
  server!();
  store.append({ type = "bump" });
}
"#;

#[test]
fn a_forged_reply_cannot_wake_another_tabs_waiter() {
    let mut host = Cluster::new(&["A", "B"], APP, false).expect("cluster");
    host.fire("A", "panel");
    host.pump();
    let parked = host.server_parked();
    assert_eq!(parked.len(), 1, "A's panel sleeps in store.wait");
    let flow = parked[0].clone();

    // B answers A's sleeping panel with a value of her choosing.
    host.inject("B", pkt(&[("kind", s("reply")), ("flow", s(&flow)), ("to", s("server")), ("value", Value::Int(666))]));
    host.inject("B", pkt(&[("kind", s("error")), ("flow", s(&flow)), ("to", s("server")), ("err", s("pwned"))]));
    // Even A herself can't answer a waiter: only the courier wakes those.
    host.inject("A", pkt(&[("kind", s("reply")), ("flow", s(&flow)), ("to", s("server")), ("value", Value::Int(1))]));
    host.pump();
    let log = host.log().join("\n");
    assert!(log.contains(&format!("reply for flow {flow} from B dropped")), "{log}");
    assert!(log.contains(&format!("error for flow {flow} from B dropped")), "{log}");
    assert!(log.contains(&format!("reply for flow {flow} from A dropped")), "{log}");
    assert_eq!(host.server_parked(), vec![flow.clone()], "still asleep, untouched");

    // A real write still wakes her, with the real value.
    host.fire("B", "bump");
    host.pump();
    let log = host.log().join("\n");
    assert!(log.contains("[browser A] n=1"), "{log}");
    assert!(!log.contains("n=666"), "{log}");
}

#[test]
fn a_forged_reply_cannot_answer_an_effect() {
    let mut host = Cluster::new(&["A", "B"], APP, false).expect("cluster");
    host.fire("A", "slow");
    // Queued behind A's call: by the time it's delivered, A's server piece
    // is parked awaiting @effects. B tries to answer for bash.
    host.inject(
        "B",
        pkt(&[
            ("kind", s("reply")),
            ("flow", s("A#1")),
            ("to", s("server")),
            ("value", pkt(&[("ok", Value::Bool(true)), ("stdout", s("forged"))])),
        ]),
    );
    host.pump();
    let log = host.log().join("\n");
    assert!(log.contains("reply for flow A#1 from B dropped: she awaits @effects"), "{log}");
    assert!(log.contains("[server] bash said real"), "{log}");
    assert!(!log.contains("bash said forged"), "{log}");
    host.assert_quiescent();
}
