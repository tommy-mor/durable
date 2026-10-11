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

// ---------------------------------------------------------------------------
// First-class `schema { … }`: map inside blobs, record outside, key kinds
// ---------------------------------------------------------------------------

const BLOCKED: &str = r#"
schema {
  todos: map<int, record { text: leaf, done: leaf }>,
  stats: record { created: sum, completed: sum }
}

fn reduce(event) {
  match event {
    add { id, text } => {
      store(["todos", id, store.set({ text = text, done = false })]);
      store(["stats", "created", store.add(1)]);
    }
  }
}
"#;

fn event(kind: &str, fields: Vec<(Value, Value)>) -> Value {
    let mut entries: Vec<(Value, Value)> = vec![(Value::str("type"), Value::str(kind))];
    entries.extend(fields);
    Value::map(entries.into_iter().collect())
}

#[test]
fn schema_block_binds_writes_and_verifies() {
    let mut host = Cluster::new(&["A"], BLOCKED, false).expect("host");
    host.append(event("add", vec![(s("id"), i(7)), (s("text"), s("milk"))]))
        .unwrap();
    let text = host.store_get(arr(vec![s("todos"), i(7), s("text")])).unwrap();
    assert_eq!(text, s("milk"));
    let created = host.store_get(arr(vec![s("stats"), s("created")])).unwrap();
    assert_eq!(created, i(1));
    host.verify().unwrap();
}

#[test]
fn schema_listing_renders_nested_generics() {
    let prog = hoprt::compiler::compile(BLOCKED).expect("compile");
    assert_eq!(
        prog.schema_listing().as_deref(),
        Some(
            "schema { todos: map<int, record { text: leaf, done: leaf }>, \
             stats: record { created: sum, completed: sum } }"
        )
    );
}

#[test]
fn map_key_violations_fail_at_append() {
    let mut host = Cluster::new(&["A"], BLOCKED, false).expect("host");
    // id is a string; todos wants int keys — the reducer fails fast.
    let err = host
        .append(event("add", vec![(s("id"), s("oops")), (s("text"), s("milk"))]))
        .expect_err("string key into map<int> should fail");
    assert!(err.contains("must be int"), "{err}");
}

const BLOBBED: &str = r#"
schema {
  threads: map<record {
    title: leaf,
    msgs: list<blob<map {
      role: string,
      text: string,
      call_id: string
    }>>
  }>
}

fn reduce(event) {
  match event {
    say { tid, text } => {
      store(["threads", tid, "msgs", store.push({ role = "user", text = text })]);
    }
    bad { tid } => {
      store(["threads", tid, "msgs", store.push({ role = "user", text = "hi", call_id = 42 })]);
    }
    extra { tid } => {
      store(["threads", tid, "msgs", store.push({ role = "user", text = "hi", nick = "bob" })]);
    }
  }
}
"#;

#[test]
fn blob_values_validate_absent_fields_and_extras() {
    let mut host = Cluster::new(&["A"], BLOBBED, false).expect("host");
    // absent call_id is fine (absent fields read as nil)…
    host.append(event("say", vec![(s("tid"), i(1)), (s("text"), s("hello"))]))
        .unwrap();
    // …and unknown fields are tolerated (forward compat)…
    host.append(event("extra", vec![(s("tid"), i(1))])).unwrap();
    let first = host
        .store_get(arr(vec![s("threads"), i(1), s("msgs"), i(0)]))
        .unwrap();
    assert_eq!(first.get_field("role"), s("user"));
    assert_eq!(first.get_field("text"), s("hello"));
    host.verify().unwrap();
}

#[test]
fn blob_shape_violations_fail_at_append() {
    let mut host = Cluster::new(&["A"], BLOBBED, false).expect("host");
    // call_id is declared string; 42 is not.
    let err = host
        .append(event("bad", vec![(s("tid"), i(1))]))
        .expect_err("int call_id into string field should fail");
    assert!(err.contains("must be string"), "{err}");
}

#[test]
fn schema_syntax_errors() {
    let cases = vec![
        (
            "unknown type",
            "schema { a: frobnicate }\nfn reduce(event) { return event; }",
            "unknown schema type",
        ),
        (
            "record in blob",
            "schema { a: blob<record { x: any }> }\nfn reduce(event) { return event; }",
            "inside blob",
        ),
        (
            "list in blob",
            "schema { a: blob<list<any>> }\nfn reduce(event) { return event; }",
            "inside blob",
        ),
        (
            "duplicate block",
            "schema { a: leaf }\nschema { b: leaf }\nfn reduce(event) { return event; }",
            "duplicate schema block",
        ),
        (
            "legacy conflict",
            "schema { a: leaf }\nserver let schema = store.leaf;\nfn reduce(event) { return event; }",
            "conflicts with `server let schema`",
        ),
    ];
    for (name, src, want) in cases {
        let err = hoprt::compiler::compile(src).unwrap_err();
        assert!(err.contains(want), "{name}: expected {want:?}, got {err:?}");
    }
}
