//! Query engine: point gets, scans, filters, project, explain, cost classes.

use ciborium::value::Integer;
use ciborium::Value;
use durable::{
    query, CostClass, Db, Describe, Durable, Durability, Expr, Leaf, List, Map, Nav, Predicate,
    Query, Shape, Sum,
};
use tempfile::TempDir;

#[derive(Durable)]
#[allow(dead_code)]
struct Store {
    events: List<Leaf<EventRow>>,
    evidence_by_id: Map<String, Leaf<EventRow>>,
    event_ids_by_kind: Map<String, List<Leaf<String>>>,
    emissions: Map<u64, Leaf<Emission>>,
    latest_emission: Leaf<Emission>,
    scores: Map<String, Sum<i64>>,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq)]
struct EventRow {
    id: String,
    kind: String,
    epoch: u64,
    payload: Payload,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq)]
struct Payload {
    split_vote: bool,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq)]
struct Emission {
    epoch: u64,
    ranking: Vec<String>,
    distributions: std::collections::BTreeMap<String, f64>,
}

fn open() -> (TempDir, Db) {
    let dir = TempDir::new().unwrap();
    let db = Db::open(dir.path()).unwrap();
    (dir, db)
}

fn seed(db: &Db) {
    let root = Store::root();
    let e1 = EventRow {
        id: "e1".into(),
        kind: "llm.judgment".into(),
        epoch: 3,
        payload: Payload { split_vote: false },
    };
    let e2 = EventRow {
        id: "e2".into(),
        kind: "llm.judgment".into(),
        epoch: 12,
        payload: Payload { split_vote: true },
    };
    let e3 = EventRow {
        id: "e3".into(),
        kind: "git.commit".into(),
        epoch: 12,
        payload: Payload { split_vote: false },
    };
    let em = Emission {
        epoch: 12,
        ranking: vec!["tommy-mor".into(), "lara".into()],
        distributions: [("tommy-mor".into(), 0.6), ("lara".into(), 0.4)]
            .into_iter()
            .collect(),
    };

    db.apply(
        &[
            root.events().push_op(&e1),
            root.events().push_op(&e2),
            root.events().push_op(&e3),
            root.evidence_by_id().key(&"e1".into()).set(&e1),
            root.evidence_by_id().key(&"e2".into()).set(&e2),
            root.evidence_by_id().key(&"e3".into()).set(&e3),
            root.event_ids_by_kind()
                .key(&"llm.judgment".into())
                .push_op(&"e1".into()),
            root.event_ids_by_kind()
                .key(&"llm.judgment".into())
                .push_op(&"e2".into()),
            root.event_ids_by_kind()
                .key(&"git.commit".into())
                .push_op(&"e3".into()),
            root.emissions().key(&12).set(&em),
            root.latest_emission().set(&em),
            root.scores().key(&"tommy-mor".into()).add(10),
            root.scores().key(&"lara".into()).add(4),
        ],
        Durability::SyncWal,
    )
    .unwrap();
}

fn text(v: &Value) -> &str {
    match v {
        Value::Text(s) => s,
        _ => panic!("expected text, got {v:?}"),
    }
}

fn map_get<'a>(v: &'a Value, key: &str) -> &'a Value {
    match v {
        Value::Map(pairs) => pairs
            .iter()
            .find(|(k, _)| matches!(k, Value::Text(s) if s == key))
            .map(|(_, val)| val)
            .unwrap(),
        _ => panic!("expected map, got {v:?}"),
    }
}

#[test]
fn one_point_get() {
    let (_dir, db) = open();
    seed(&db);
    let q = Query::new(vec![
        Nav::Field("evidence_by_id".into()),
        Nav::Key(Value::Text("e2".into())),
        Nav::Field("kind".into()),
    ]);
    let got = query::one(&db, &Store::shape(), &q).unwrap().unwrap();
    assert_eq!(text(&got), "llm.judgment");
}

#[test]
fn one_rejects_collecting_paths() {
    let (_dir, db) = open();
    seed(&db);
    let q = Query::new(vec![Nav::Field("emissions".into()), Nav::All]);
    assert!(query::one(&db, &Store::shape(), &q).is_err());
}

#[test]
fn select_kinds_is_a_scan() {
    let (_dir, db) = open();
    seed(&db);
    let q = Query::new(vec![
        Nav::Field("events".into()),
        Nav::All,
        Nav::Field("kind".into()),
    ]);
    let kinds = query::select(&db, &Store::shape(), &q).unwrap();
    let kinds: Vec<&str> = kinds.iter().map(text).collect();
    assert_eq!(kinds, vec!["llm.judgment", "llm.judgment", "git.commit"]);

    let plan = query::explain(&Store::shape(), &q).unwrap();
    assert_eq!(plan.class, CostClass::Scan);
    assert!(plan.to_string().contains("PrefixScan"));
}

#[test]
fn where_filters_leaf_interiors() {
    let (_dir, db) = open();
    seed(&db);
    let q = Query::new(vec![
        Nav::Field("events".into()),
        Nav::Where(Predicate::Ge(
            Expr::Field("epoch".into()),
            Expr::Lit(Value::Integer(Integer::from(10u64))),
        )),
        Nav::Field("id".into()),
    ]);
    let ids = query::select(&db, &Store::shape(), &q).unwrap();
    let ids: Vec<&str> = ids.iter().map(text).collect();
    assert_eq!(ids, vec!["e2", "e3"]);
}

#[test]
fn nested_where_on_payload() {
    let (_dir, db) = open();
    seed(&db);
    let q = Query::new(vec![
        Nav::Field("events".into()),
        Nav::Where(Predicate::Eq(
            Expr::Path(vec![
                Nav::Field("payload".into()),
                Nav::Field("split_vote".into()),
            ]),
            Expr::Lit(Value::Bool(true)),
        )),
        Nav::Field("id".into()),
    ]);
    let ids = query::select(&db, &Store::shape(), &q).unwrap();
    assert_eq!(ids.iter().map(text).collect::<Vec<_>>(), vec!["e2"]);
}

#[test]
fn subtree_and_project() {
    let (_dir, db) = open();
    seed(&db);
    let latest = query::subtree(
        &db,
        &Store::shape(),
        &Query::new(vec![Nav::Field("latest_emission".into())]),
    )
    .unwrap();
    assert_eq!(
        map_get(&latest, "epoch"),
        &Value::Integer(Integer::from(12u64))
    );

    let projected = query::project(
        &db,
        &Store::shape(),
        &[
            (
                "latest".into(),
                Query::new(vec![Nav::Field("latest_emission".into())]),
            ),
            (
                "kinds".into(),
                Query::new(vec![
                    Nav::Field("events".into()),
                    Nav::All,
                    Nav::Field("kind".into()),
                ]),
            ),
        ],
    )
    .unwrap();
    let kinds = map_get(&projected, "kinds");
    match kinds {
        Value::Array(xs) => assert_eq!(xs.len(), 3),
        other => panic!("{other:?}"),
    }
}

#[test]
fn entries_and_sum() {
    let (_dir, db) = open();
    seed(&db);
    let pairs = query::entries(
        &db,
        &Store::shape(),
        &Query::new(vec![Nav::Field("scores".into())]),
    )
    .unwrap();
    assert_eq!(pairs.len(), 2);
    let tommy = query::one(
        &db,
        &Store::shape(),
        &Query::new(vec![
            Nav::Field("scores".into()),
            Nav::Key(Value::Text("tommy-mor".into())),
        ]),
    )
    .unwrap()
    .unwrap();
    assert_eq!(tommy, Value::Integer(Integer::from(10i64)));
}

#[test]
fn explain_point_vs_scan() {
    let point = Query::new(vec![
        Nav::Field("evidence_by_id".into()),
        Nav::Key(Value::Text("e1".into())),
    ]);
    let scan = Query::new(vec![
        Nav::Field("emissions".into()),
        Nav::All,
        Nav::Field("distributions".into()),
        Nav::Key(Value::Text("tommy-mor".into())),
    ]);
    assert_eq!(
        query::explain(&Store::shape(), &point).unwrap().class,
        CostClass::Point
    );
    let plan = query::explain(&Store::shape(), &scan).unwrap();
    assert_eq!(plan.class, CostClass::Scan);
    assert!(plan.to_string().contains("estimated class: SCAN"));

    let filter = Query::new(vec![
        Nav::Field("events".into()),
        Nav::Where(Predicate::Eq(
            Expr::Field("id".into()),
            Expr::Lit(Value::Text("e1".into())),
        )),
    ]);
    let hinted = query::explain(&Store::shape(), &filter).unwrap();
    assert_eq!(hinted.class, CostClass::Scan);
    assert!(hinted
        .to_string()
        .contains("a Map keyed by id is POINT"));
}

#[test]
fn shape_describe_matches_manual() {
    let expected = Shape::record(vec![
        ("events".into(), Shape::list(Shape::Leaf)),
        ("evidence_by_id".into(), Shape::map(Shape::Leaf)),
        ("event_ids_by_kind".into(), Shape::map(Shape::list(Shape::Leaf))),
        ("emissions".into(), Shape::map(Shape::Leaf)),
        ("latest_emission".into(), Shape::Leaf),
        ("scores".into(), Shape::map(Shape::Sum)),
    ]);
    assert_eq!(Store::shape(), expected);
}
