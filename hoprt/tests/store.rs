//! Durable store: hop todo events land on the JSONL tape, survive reopen,
//! and incremental execution equals replay from zero.

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

const TODO: &str = include_str!("../hop/todo.hop");

#[test]
fn todo_persists_across_reopen_and_verifies() {
    let dir = tempfile::tempdir().unwrap();

    {
        let mut host = Cluster::with_data_dir(&["A"], TODO, false, dir.path()).expect("host");
        host.fire("A", "sim_demo");
        host.pump();
        host.assert_quiescent();

        assert_eq!(host.applied().unwrap(), 2);
        let milk = host.store_get(arr(vec![s("todos"), i(0), s("text")])).unwrap();
        assert_eq!(milk, s("buy milk"));
        host.verify().unwrap();
    }

    // New process, same tape: catch-up restores the projection.
    let mut host = Cluster::with_data_dir(&["A"], TODO, false, dir.path()).expect("reopen");
    assert_eq!(host.applied().unwrap(), 2);
    let milk = host.store_get(arr(vec![s("todos"), i(0), s("text")])).unwrap();
    assert_eq!(milk, s("buy milk"));
    let second = host.store_get(arr(vec![s("todos"), i(1), s("text")])).unwrap();
    assert_eq!(second, s("write the compiler"));
    host.verify().unwrap();

    let log = std::fs::read_to_string(dir.path().join("log.jsonl")).unwrap();
    assert!(log.contains(r#""type":"add""#), "{log}");
    assert_eq!(log.lines().filter(|l| !l.trim().is_empty()).count(), 2);
}

// where/slice are parametrized navigators: plain data in the path.
const FILTERED: &str = r#"
server let schema = store.record([
  ["todos", store.map(store.record([
    ["text", store.leaf],
    ["done", store.leaf]
  ]))]
]);

fn reduce(event) {
  if event.type == "add" {
    store(["todos", event.seq, store.set({ text = event.text, done = event.done })]);
  }
}

fn open_texts() {
  return store(["todos", store.where("done", false), "text"]);
}

fn late_texts() {
  return store(["todos", store.slice(1), "text"]);
}
"#;

#[test]
fn where_and_slice_navigators_filter_queries() {
    let mut host = Cluster::new(&["A"], FILTERED, false).expect("host");
    for (text, done) in [("milk", true), ("eggs", false), ("bread", false)] {
        host.append(Value::map(
            [
                (s("type"), s("add")),
                (s("text"), s(text)),
                (s("done"), Value::Bool(done)),
            ]
            .into_iter()
            .collect(),
        ))
        .unwrap();
    }

    let open = host.call_server("open_texts", vec![]).unwrap();
    assert_eq!(open, arr(vec![s("eggs"), s("bread")]));

    let late = host.call_server("late_texts", vec![]).unwrap();
    assert_eq!(late, arr(vec![s("eggs"), s("bread")]));
}

#[test]
fn rebuild_from_tape_matches_live() {
    let mut host = Cluster::new(&["A"], TODO, false).expect("host");
    host.fire("A", "sim_demo");
    host.pump();
    host.rebuild().unwrap();
    host.verify().unwrap();
    let created = host.store_get(arr(vec![s("stats"), s("created")])).unwrap();
    assert_eq!(created, i(2));
}
