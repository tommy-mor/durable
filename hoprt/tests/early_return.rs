//! Early return across hops: a far piece's `return` stops the caller.
//!
//! A far piece's reply used to mean "resume my continuation" no matter
//! what, so `return` past a mark fell through into the caller's
//! continuation. Pieces that can return now tag their replies
//! (`{ ret, value, vars }`); every mark into such a piece checks the tag
//! and either returns the value up a level or resumes normally. Pieces
//! that can't return keep the old `{ value, vars }` shape exactly.

use hoprt::harness::Cluster;
use hoprt::value::Value;

fn run(src: &str, entry: &str, args: Vec<Value>) -> Vec<String> {
    let mut host = Cluster::new(&["A"], src, false).expect("cluster");
    host.fire_args("A", entry, args);
    host.pump();
    host.assert_quiescent();
    host.log()
}

fn has(log: &[String], needle: &str) {
    let all = log.join("\n");
    assert!(all.contains(needle), "missing {needle:?}:\n{all}");
}

fn missing(log: &[String], needle: &str) {
    let all = log.join("\n");
    assert!(!all.contains(needle), "unexpected {needle:?}:\n{all}");
}

const RETURNS: &str = r#"
fn go_a(c) {
  if c {
    server!();
    print("a-server");
    return 1;
  }
  print("a-origin");
}

fn go_b(xs) {
  for k, v in xs {
    server!();
    print("hop v=" .. v);
    if v > 2 {
      print("b-done");
      return v;
    }
  }
  print("b-zero");
}

fn go_c() {
  server!();
  print("c-server");
  browser!();
  print("c-browser");
  return 7;
  server!();
}

fn go_f(x) {
  let y = x;
  if y > 0 {
    server!();
    y = y * 2;
    if y > 10 {
      return y;
    }
  }
  print("f y=" .. y);
}
"#;

#[test]
fn return_in_a_branch_stops_the_caller() {
    let log = run(RETURNS, "go_a", vec![Value::Bool(true)]);
    has(&log, "a-server");
    missing(&log, "a-origin");
    // the value rode the tagged reply, not the continuation
    has(&log, "value={ret: true, value: 1, vars: {}}");

    let log = run(RETURNS, "go_a", vec![Value::Bool(false)]);
    has(&log, "a-origin");
    missing(&log, "a-server");
}

#[test]
fn return_in_a_loop_hops_once() {
    let xs = Value::array(vec![Value::Int(1), Value::Int(3), Value::Int(5)]);
    let log = run(RETURNS, "go_b", vec![xs]);
    has(&log, "hop v=1");
    has(&log, "hop v=3");
    has(&log, "b-done");
    // the return stopped the scan: no third hop, no fallthrough
    missing(&log, "hop v=5");
    missing(&log, "b-zero");
    assert_eq!(
        log.iter().filter(|l| l.contains("hop v=")).count(),
        2,
        "expected exactly two hops:\n{}",
        log.join("\n")
    );
    has(&log, "ret: true, value: 3");
}

#[test]
fn pop_back_to_origin_needs_no_tag() {
    // `browser!()` here returns to the origin piece (there is no separate
    // browser hop), so nothing can return past a mark and the untagged
    // shape stays — but the dead mark after `return` must not break the
    // build or duplicate the prints.
    let log = run(RETURNS, "go_c", vec![]);
    assert_eq!(
        log.iter().filter(|l| l.contains("c-server")).count(),
        1,
        "c-server printed twice:\n{}",
        log.join("\n")
    );
    assert_eq!(
        log.iter().filter(|l| l.contains("c-browser")).count(),
        1,
        "c-browser printed twice:\n{}",
        log.join("\n")
    );
    missing(&log, "ret:");
}

#[test]
fn return_and_reply_vars_share_one_tag() {
    // the far piece both rebinds `y` for the continuation AND can return:
    // ret:false resumes with the rebound value, ret:true skips it.
    let log = run(RETURNS, "go_f", vec![Value::Int(3)]);
    has(&log, "f y=6");
    has(&log, "ret: false");

    let log = run(RETURNS, "go_f", vec![Value::Int(10)]);
    missing(&log, "f y=");
    has(&log, "ret: true, value: 20");
}
