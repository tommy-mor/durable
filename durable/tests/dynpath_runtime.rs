//! Dynamic paths + JSON events: the Lua/hop surface, exercised in Rust.

use ciborium::value::Integer;
use ciborium::Value;
use durable::{dynpath, Nav, Query, Runtime, Shape, Tx};
use serde_json::json;
use tempfile::TempDir;

fn schema() -> Shape {
    Shape::record(vec![
        (
            "todos".into(),
            Shape::map(Shape::record(vec![
                ("text".into(), Shape::Leaf),
                ("done".into(), Shape::Leaf),
            ])),
        ),
        (
            "stats".into(),
            Shape::record(vec![
                ("created".into(), Shape::Sum),
                ("completed".into(), Shape::Sum),
            ]),
        ),
    ])
}

fn reduce(tx: &mut Tx, event: &serde_json::Value) -> durable::Result<()> {
    let s = schema();
    match event["type"].as_str() {
        Some("add") => {
            let id = Value::Integer(Integer::from(tx.seq() as i64));
            tx.put(
                &s,
                &[Nav::Field("todos".into()), Nav::Key(id)],
                &dynpath::json_to_cbor(&json!({
                    "text": event["text"],
                    "done": false,
                })),
            )?;
            tx.add(
                &s,
                &[Nav::Field("stats".into()), Nav::Field("created".into())],
                &Value::Integer(Integer::from(1i64)),
            )?;
        }
        Some("toggle") => {
            let id = dynpath::json_to_cbor(&event["id"]);
            let done_navs = vec![
                Nav::Field("todos".into()),
                Nav::Key(id.clone()),
                Nav::Field("done".into()),
            ];
            let done = tx.peek(&s, &done_navs)?;
            let next = !matches!(done, Value::Bool(true));
            tx.put(&s, &done_navs, &Value::Bool(next))?;
            tx.add(
                &s,
                &[
                    Nav::Field("stats".into()),
                    Nav::Field("completed".into()),
                ],
                &Value::Integer(Integer::from(if next { 1i64 } else { -1 })),
            )?;
        }
        other => return Err(durable::Error::Reducer(format!("unknown event {other:?}"))),
    }
    Ok(())
}

fn open(dir: &TempDir) -> Runtime<serde_json::Value> {
    Runtime::open(
        dir.path().join("proj"),
        dir.path().join("log.jsonl"),
        schema(),
        None,
        reduce,
    )
    .unwrap()
}

#[test]
fn json_events_index_and_replay() {
    let dir = TempDir::new().unwrap();
    let mut rt = open(&dir);
    rt.append(json!({"type": "add", "text": "milk"})).unwrap();
    rt.append(json!({"type": "add", "text": "compiler"})).unwrap();
    rt.append(json!({"type": "toggle", "id": 0})).unwrap();

    let text = rt
        .one(&Query::new(vec![
            Nav::Field("todos".into()),
            Nav::Key(Value::Integer(Integer::from(0i64))),
            Nav::Field("text".into()),
        ]))
        .unwrap()
        .unwrap();
    assert!(matches!(text, Value::Text(s) if s == "milk"));

    let done = rt
        .one(&Query::new(vec![
            Nav::Field("todos".into()),
            Nav::Key(Value::Integer(Integer::from(0i64))),
            Nav::Field("done".into()),
        ]))
        .unwrap()
        .unwrap();
    assert_eq!(done, Value::Bool(true));

    let created = rt
        .one(&Query::new(vec![
            Nav::Field("stats".into()),
            Nav::Field("created".into()),
        ]))
        .unwrap()
        .unwrap();
    assert_eq!(created, Value::Integer(Integer::from(2i64)));

    rt.verify().unwrap();
    rt.rebuild().unwrap();
    rt.verify().unwrap();
}

#[test]
fn reopen_catches_up_from_jsonl() {
    let dir = TempDir::new().unwrap();
    {
        let mut rt = open(&dir);
        rt.append(json!({"type": "add", "text": "persist me"})).unwrap();
    }
    let rt = open(&dir);
    assert_eq!(rt.applied().unwrap(), 1);
    let text = rt
        .one(&Query::new(vec![
            Nav::Field("todos".into()),
            Nav::Key(Value::Integer(Integer::from(0i64))),
            Nav::Field("text".into()),
        ]))
        .unwrap()
        .unwrap();
    assert!(matches!(text, Value::Text(s) if s == "persist me"));
    rt.verify().unwrap();
}
