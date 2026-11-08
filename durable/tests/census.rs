//! `census`, `status`, `verify`, `cat` and the `durable-tape` binary: hand
//! counts, an independent oracle, kind detection per serde tagging, JSON
//! stability, exit codes, and read-only behaviour.

mod common;

use std::collections::BTreeMap;
use std::process::Command;

use common::*;
use durable::census::{self, detect_kind};
use proptest::prelude::*;
use serde_json::json;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_durable-tape")
}

/// The ladder-shaped fixture: 3 batches, 6 records, known kinds and fields.
fn fixture(dir: &std::path::Path) -> std::path::PathBuf {
    let path = dir.join("fixture.jsonl");
    let lines = vec![
        sealed_raw(0, 100, 0, 0, r#"{"type":"joined","id":"u1","name":"a"}"#),
        sealed_raw(
            1,
            101,
            1,
            2,
            r#"{"type":"round_began","round":1,"map":"m"}"#,
        ),
        sealed_raw(2, 102, 1, 2, r#"{"type":"duel_resolved","winner":"u1"}"#),
        sealed_raw(3, 103, 3, 5, r#"{"type":"joined","id":"u2"}"#),
        sealed_raw(4, 104, 3, 5, r#"{"type":"left","id":"u2"}"#),
        sealed_raw(5, 105, 3, 5, r#"{"type":"round_began","round":2}"#),
    ];
    write_lines(&path, &lines);
    path
}

#[test]
fn hand_counted_fixture() {
    let dir = tmp_dir("census-hand");
    let path = fixture(&dir);
    let c = census::census(&path, "type").unwrap();
    assert_eq!(c.records, 6);
    assert_eq!(c.batches, 3);
    assert_eq!(c.first_seq, Some(0));
    assert_eq!(c.last_seq, Some(5));
    assert_eq!((c.ts_min, c.ts_max), (Some(100), Some(105)));
    assert_eq!(c.batch_sizes, BTreeMap::from([(1, 1), (2, 1), (3, 1)]));
    assert_eq!(c.untagged, 0);
    assert_eq!(c.kinds["joined"].count, 2);
    assert_eq!(
        c.kinds["joined"].fields,
        BTreeMap::from([("id".to_string(), 2), ("name".to_string(), 1)])
    );
    assert_eq!(c.kinds["round_began"].count, 2);
    assert_eq!(
        c.kinds["round_began"].fields,
        BTreeMap::from([("map".to_string(), 1), ("round".to_string(), 2)])
    );
    assert_eq!(c.kinds["duel_resolved"].count, 1);
    assert_eq!(c.kinds["left"].count, 1);
    assert_eq!(c.tail_bytes, 0);
    assert_eq!(c.committed_bytes, std::fs::metadata(&path).unwrap().len());
    let byte_sum: u64 = c.kinds.values().map(|k| k.event_bytes).sum();
    assert_eq!(byte_sum, c.event_bytes);
}

#[test]
fn json_is_stable_and_machine_readable() {
    let dir = tmp_dir("census-json");
    let path = fixture(&dir);
    let c = census::census(&path, "type").unwrap();
    let j = c.to_json();
    assert_eq!(j["schema"], "durable-census/1");
    assert_eq!(j["records"], 6);
    assert_eq!(j["batch_sizes"], json!({"1": 1, "2": 1, "3": 1}));
    assert_eq!(j["kinds"]["joined"]["count"], 2);
    assert_eq!(j["seq"], json!({"first": 0, "last": 5}));
    // Same input, same bytes.
    assert_eq!(
        j.to_string(),
        census::census(&path, "type").unwrap().to_json().to_string()
    );
    let table = c.to_table();
    assert!(
        table.contains("joined") && table.contains("round_began"),
        "{table}"
    );
}

#[test]
fn detection_by_serde_tagging() {
    use serde_json::Value;
    let internal: Value = serde_json::from_str(r#"{"type":"add","key":"a","n":1}"#).unwrap();
    assert_eq!(
        detect_kind(&internal, "type"),
        (Some("add".into()), vec!["key".into(), "n".into()])
    );
    let unit_internal: Value = serde_json::from_str(r#"{"type":"reset"}"#).unwrap();
    assert_eq!(
        detect_kind(&unit_internal, "type"),
        (Some("reset".into()), vec![])
    );
    let external_struct: Value = serde_json::from_str(r#"{"Add":{"key":"a","n":1}}"#).unwrap();
    assert_eq!(
        detect_kind(&external_struct, "type"),
        (Some("Add".into()), vec!["key".into(), "n".into()])
    );
    let external_unit: Value = serde_json::from_str(r#""Reset""#).unwrap();
    assert_eq!(
        detect_kind(&external_unit, "type"),
        (Some("Reset".into()), vec![])
    );
    let external_newtype: Value = serde_json::from_str(r#"{"Note":"hi"}"#).unwrap();
    assert_eq!(
        detect_kind(&external_newtype, "type"),
        (Some("Note".into()), vec![])
    );
    let adjacent: Value = serde_json::from_str(r#"{"t":"add","c":{"key":"a"}}"#).unwrap();
    assert_eq!(
        detect_kind(&adjacent, "t"),
        (Some("add".into()), vec!["c".into()])
    );
    let untagged: Value = serde_json::from_str(r#"{"a":1,"b":2}"#).unwrap();
    assert_eq!(
        detect_kind(&untagged, "type"),
        (None, vec!["a".into(), "b".into()])
    );
    let number: Value = serde_json::from_str("42").unwrap();
    assert_eq!(detect_kind(&number, "type"), (None, vec![]));
}

#[test]
fn census_tag_flag_selects_the_tag_key() {
    let dir = tmp_dir("census-tagflag");
    let path = dir.join("t.jsonl");
    write_lines(
        &path,
        &[sealed_raw(0, 1, 0, 0, r#"{"t":"go","c":{"x":1}}"#)],
    );
    let c = census::census(&path, "t").unwrap();
    assert_eq!(c.kinds["go"].count, 1);
    assert_eq!(c.kinds["go"].fields, BTreeMap::from([("c".to_string(), 1)]));
}

#[test]
fn untagged_events_are_counted_not_guessed() {
    let dir = tmp_dir("census-untagged");
    let path = dir.join("u.jsonl");
    write_lines(
        &path,
        &[
            sealed_raw(0, 1, 0, 0, r#"{"a":1,"b":2}"#),
            sealed_raw(1, 2, 1, 1, "7"),
        ],
    );
    let c = census::census(&path, "type").unwrap();
    assert_eq!(c.untagged, 2);
    assert!(c.kinds.is_empty());
}

#[test]
fn torn_tail_is_reported_and_the_file_is_not_touched() {
    let dir = tmp_dir("census-torn");
    let path = fixture(&dir);
    let mut bytes = read_bytes(&path);
    bytes.extend_from_slice(br#"{"seq":6,"ts_ms":106,"from":6,"to":6,"event":{"ty"#);
    std::fs::write(&path, &bytes).unwrap();
    let before = read_bytes(&path);
    let c = census::census(&path, "type").unwrap();
    assert_eq!(c.records, 6);
    assert!(c.tail_bytes > 0);
    assert_eq!(read_bytes(&path), before, "census never writes");
    let s = census::status(&path).unwrap();
    assert_eq!(s.committed.len, 6);
    assert_eq!(read_bytes(&path), before);
}

#[test]
fn corrupt_tape_is_an_error_not_a_guess() {
    let dir = tmp_dir("census-corrupt");
    let path = fixture(&dir);
    let mut bytes = read_bytes(&path);
    let i = bytes.len() / 2;
    bytes[i] ^= 0x20;
    std::fs::write(&path, &bytes).unwrap();
    assert!(census::census(&path, "type").is_err());
    assert!(census::verify(&path).is_err());
}

#[test]
fn cat_returns_committed_lines_from_a_seq() {
    let dir = tmp_dir("census-cat");
    let path = fixture(&dir);
    let lines = census::cat(&path, 3).unwrap();
    assert_eq!(lines.len(), 3);
    assert!(String::from_utf8(lines[0].clone())
        .unwrap()
        .starts_with(r#"{"seq":3,"#));
}

// ---- independent oracle ----

#[derive(Debug, Clone)]
struct Oracle {
    kinds: BTreeMap<String, (u64, u64, BTreeMap<String, u64>)>,
    batches: BTreeMap<u64, u64>,
    ts: (u64, u64),
    event_bytes: u64,
    records: u64,
}

fn kind_strategy() -> impl Strategy<Value = (String, Vec<String>, u64)> {
    (
        prop_oneof![Just("alpha"), Just("beta"), Just("gamma_x"), Just("d")].prop_map(String::from),
        proptest::collection::vec(
            prop_oneof![Just("f1"), Just("f2"), Just("zz"), Just("name")].prop_map(String::from),
            0..4,
        ),
        any::<u32>().prop_map(u64::from),
    )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(96))]

    /// census == an oracle computed in the test from the generating data:
    /// per-kind counts, per-field presence, batch histogram, ts range, bytes.
    #[test]
    fn census_matches_an_independent_oracle(
        batches in proptest::collection::vec(
            proptest::collection::vec((kind_strategy(), any::<u32>()), 1..6),
            1..6,
        ),
        tag_style in 0u8..2,
    ) {
        let dir = tmp_dir("census-oracle");
        let path = dir.join("o.jsonl");
        let mut oracle = Oracle {
            kinds: BTreeMap::new(),
            batches: BTreeMap::new(),
            ts: (u64::MAX, 0),
            event_bytes: 0,
            records: 0,
        };
        let mut lines = Vec::new();
        let mut seq = 0u64;
        for batch in &batches {
            let from = seq;
            let to = seq + batch.len() as u64 - 1;
            *oracle.batches.entry(batch.len() as u64).or_insert(0) += 1;
            for ((kind, fields, n), ts) in batch {
                let mut obj = serde_json::Map::new();
                if tag_style == 0 {
                    obj.insert("type".into(), json!(kind));
                    for f in fields.iter().collect::<std::collections::BTreeSet<_>>() {
                        obj.insert(f.clone(), json!(n));
                    }
                    let raw = serde_json::to_string(&obj).unwrap();
                    oracle.event_bytes += raw.len() as u64;
                    let entry = oracle.kinds.entry(kind.clone()).or_insert((0, 0, BTreeMap::new()));
                    entry.0 += 1;
                    entry.1 += raw.len() as u64;
                    for f in obj.keys().filter(|k| k.as_str() != "type") {
                        *entry.2.entry(f.clone()).or_insert(0) += 1;
                    }
                    lines.push(sealed_raw(seq, *ts as u64, from, to, &raw));
                } else {
                    // Externally tagged: {"<kind>": {fields}}.
                    let mut inner = serde_json::Map::new();
                    for f in fields.iter().collect::<std::collections::BTreeSet<_>>() {
                        inner.insert(f.clone(), json!(n));
                    }
                    let raw = serde_json::to_string(&json!({ kind.clone(): inner })).unwrap();
                    oracle.event_bytes += raw.len() as u64;
                    let entry = oracle.kinds.entry(kind.clone()).or_insert((0, 0, BTreeMap::new()));
                    entry.0 += 1;
                    entry.1 += raw.len() as u64;
                    for f in inner.keys() {
                        *entry.2.entry(f.clone()).or_insert(0) += 1;
                    }
                    lines.push(sealed_raw(seq, *ts as u64, from, to, &raw));
                }
                oracle.ts.0 = oracle.ts.0.min(*ts as u64);
                oracle.ts.1 = oracle.ts.1.max(*ts as u64);
                oracle.records += 1;
                seq += 1;
            }
        }
        write_lines(&path, &lines);
        let c = census::census(&path, "type").unwrap();
        prop_assert_eq!(c.records, oracle.records);
        prop_assert_eq!(c.batch_sizes.clone(), oracle.batches.clone());
        prop_assert_eq!((c.ts_min, c.ts_max), (Some(oracle.ts.0), Some(oracle.ts.1)));
        prop_assert_eq!(c.event_bytes, oracle.event_bytes);
        prop_assert_eq!(c.untagged, 0);
        prop_assert_eq!(c.kinds.len(), oracle.kinds.len());
        for (kind, (count, bytes, fields)) in &oracle.kinds {
            let got = &c.kinds[kind];
            prop_assert_eq!(got.count, *count, "count {}", kind);
            prop_assert_eq!(got.event_bytes, *bytes, "bytes {}", kind);
            prop_assert_eq!(&got.fields, fields, "fields {}", kind);
        }
    }
}

// ---- the binary ----

#[test]
fn cli_exit_codes_and_outputs() {
    let dir = tmp_dir("census-cli");
    let path = fixture(&dir);
    let p = path.to_str().unwrap();

    let out = Command::new(bin()).args(["status", p]).output().unwrap();
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(
        text.contains("records: 6") && text.contains("last_seq: 5"),
        "{text}"
    );

    let out = Command::new(bin()).args(["verify", p]).output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert!(String::from_utf8(out.stdout)
        .unwrap()
        .starts_with("ok: 6 records"));

    let out = Command::new(bin())
        .args(["cat", p, "--from", "4"])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(String::from_utf8(out.stdout).unwrap().lines().count(), 2);

    let out = Command::new(bin())
        .args(["census", p, "--json"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["records"], 6);

    let out = Command::new(bin()).args(["census", p]).output().unwrap();
    assert!(String::from_utf8(out.stdout)
        .unwrap()
        .contains("round_began"));

    let out = Command::new(bin()).args(["bogus"]).output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    let out = Command::new(bin()).args(["status"]).output().unwrap();
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn cli_verify_reports_torn_tail_with_code_four_and_corruption_with_one() {
    let dir = tmp_dir("census-cli-codes");
    let path = fixture(&dir);
    let mut bytes = read_bytes(&path);
    bytes.extend_from_slice(b"{\"seq\":6,\"ts");
    std::fs::write(&path, &bytes).unwrap();
    let out = Command::new(bin())
        .args(["verify", path.to_str().unwrap()])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(4), "torn tail is not clean");

    let mut bytes = read_bytes(&fixture(&dir));
    let i = bytes.len() / 3;
    bytes[i] ^= 0x01;
    std::fs::write(&path, &bytes).unwrap();
    let out = Command::new(bin())
        .args(["verify", path.to_str().unwrap()])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "corruption is an error");
}
