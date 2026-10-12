//! Marks inside branches, arms, and loop bodies that hop back to the side
//! the body was entered on.
//!
//! A body entered in piece `p` (an `if` branch, a `match` arm, a loop body)
//! has to rejoin `p`: its join block or loop header lives there. A mark
//! back to the side that parked `p` used to *close* `p`, leaving the join
//! or back-edge stranded in a piece that had already replied, and the
//! emitter panicked ("no entry found for key"). Inside a nested body such a
//! mark now opens a nested piece instead: `p` stays parked, the body runs
//! over there, and control comes home to `p` when the body ends or marks
//! back. Straight-line code at the top of a function keeps the linear
//! close-and-continue shape (and its exact ship sets).
//!
//! Assertions are on transcripts: which side printed what, in what order,
//! and which values came home.

use hoprt::harness::Cluster;
use hoprt::value::Value;

fn run(src: &str, entry: &str, args: Vec<Value>) -> Vec<String> {
    let mut host = Cluster::new(&["A"], src, false).expect("cluster");
    host.fire_args("A", entry, args);
    host.pump();
    host.assert_quiescent();
    host.log()
}

/// The prints (and only the prints), in order.
fn prints(log: &[String]) -> Vec<String> {
    log.iter()
        .filter(|l| l.starts_with("[server]") || l.starts_with("[browser A]"))
        .cloned()
        .collect()
}

fn expect(log: &[String], want: &[&str]) {
    let got = prints(log);
    let want: Vec<String> = want.iter().map(|s| s.to_string()).collect();
    assert_eq!(got, want, "\nfull transcript:\n{}", log.join("\n"));
}

const SRC: &str = r#"
fn if_back(c) {
  server!();
  let x = 1;
  if c {
    browser!();
    print("in branch x=" .. x);
    x = x + 10;
  }
  print("after x=" .. x);
}

fn if_back_both(c) {
  server!();
  let x = 1;
  if c {
    browser!();
    x = x + 10;
  } else {
    browser!();
    x = x + 20;
  }
  print("after x=" .. x);
}

fn if_back_and_forth(c) {
  server!();
  if c {
    browser!();
    print("b");
    server!();
    print("s in branch");
  }
  print("s after");
}

fn arm_back(e) {
  server!();
  let seen = "none";
  match e {
    a { n } => {
      browser!();
      print("arm a n=" .. n);
      seen = "a";
    }
    else => {
      seen = "other";
    }
  }
  print("seen=" .. seen);
}

fn for_back(xs) {
  server!();
  let total = 0;
  for i, x in xs {
    browser!();
    print("b x=" .. x);
    total = total + x;
  }
  print("total=" .. total);
}

fn for_back_and_forth(xs) {
  server!();
  for i, x in xs {
    browser!();
    print("b " .. x);
    server!();
    print("s " .. x);
  }
  print("done");
}

fn while_true_back() {
  server!();
  let n = 0;
  while true {
    n = n + 1;
    print("s n=" .. n);
    browser!();
    print("b n=" .. n);
    if n == 3 {
      return n;
    }
  }
}

fn calls_while_true_back() {
  let r = while_true_back();
  print("returned " .. r);
}

fn panel_shape(rounds) {
  server!();
  let i = 0;
  while i < rounds {
    let snap = "frame " .. i;
    browser!();
    print("render " .. snap);
    server!();
    i = i + 1;
  }
  print("panel ends");
}

fn deep(xs, c) {
  server!();
  for i, x in xs {
    if c {
      browser!();
      for j, y in [10, 20] {
        server!();
        print("s " .. x .. "/" .. y);
      }
      print("b " .. x);
    }
  }
  print("deep done");
}

fn top_level_linear() {
  server!();
  print("s1");
  browser!();
  print("b1");
  server!();
  print("s2");
}
"#;

#[test]
fn if_branch_hops_back_and_rejoins() {
    let log = run(SRC, "if_back", vec![Value::Bool(true)]);
    expect(&log, &["[browser A] in branch x=1", "[server] after x=11"]);
    let log = run(SRC, "if_back", vec![Value::Bool(false)]);
    expect(&log, &["[server] after x=1"]);
}

#[test]
fn both_branches_hop_back() {
    expect(&run(SRC, "if_back_both", vec![Value::Bool(true)]), &["[server] after x=11"]);
    expect(&run(SRC, "if_back_both", vec![Value::Bool(false)]), &["[server] after x=21"]);
}

#[test]
fn branch_hops_back_then_forward() {
    let log = run(SRC, "if_back_and_forth", vec![Value::Bool(true)]);
    expect(&log, &["[browser A] b", "[server] s in branch", "[server] s after"]);
}

#[test]
fn match_arm_hops_back() {
    let ev = Value::map(
        [
            (Value::str("type"), Value::str("a")),
            (Value::str("n"), Value::Int(5)),
        ]
        .into_iter()
        .collect(),
    );
    let log = run(SRC, "arm_back", vec![ev]);
    expect(&log, &["[browser A] arm a n=5", "[server] seen=a"]);
}

#[test]
fn for_body_hops_back_every_iteration() {
    let xs = Value::array(vec![Value::Int(1), Value::Int(2), Value::Int(3)]);
    let log = run(SRC, "for_back", vec![xs]);
    expect(
        &log,
        &["[browser A] b x=1", "[browser A] b x=2", "[browser A] b x=3", "[server] total=6"],
    );
}

#[test]
fn for_body_hops_back_and_forth() {
    let xs = Value::array(vec![Value::Int(1), Value::Int(2)]);
    let log = run(SRC, "for_back_and_forth", vec![xs]);
    expect(
        &log,
        &["[browser A] b 1", "[server] s 1", "[browser A] b 2", "[server] s 2", "[server] done"],
    );
}

#[test]
fn while_true_hops_back_and_returns_through_nesting() {
    let log = run(SRC, "calls_while_true_back", vec![]);
    expect(
        &log,
        &[
            "[server] s n=1",
            "[browser A] b n=1",
            "[server] s n=2",
            "[browser A] b n=2",
            "[server] s n=3",
            "[browser A] b n=3",
            "[browser A] returned 3",
        ],
    );
}

#[test]
fn panel_shaped_loop_renders_each_round() {
    let log = run(SRC, "panel_shape", vec![Value::Int(3)]);
    expect(
        &log,
        &[
            "[browser A] render frame 0",
            "[browser A] render frame 1",
            "[browser A] render frame 2",
            "[server] panel ends",
        ],
    );
}

#[test]
fn nested_loops_and_branches_hop_both_ways() {
    let xs = Value::array(vec![Value::Int(1), Value::Int(2)]);
    let log = run(SRC, "deep", vec![xs, Value::Bool(true)]);
    expect(
        &log,
        &[
            "[server] s 1/10",
            "[server] s 1/20",
            "[browser A] b 1",
            "[server] s 2/10",
            "[server] s 2/20",
            "[browser A] b 2",
            "[server] deep done",
        ],
    );
}

#[test]
fn top_level_marks_stay_linear() {
    // Straight-line code still closes and continues: two hops out, two
    // replies home, no nested pieces — the ship sets from before the fix.
    let log = run(SRC, "top_level_linear", vec![]);
    expect(&log, &["[server] s1", "[browser A] b1", "[server] s2"]);
    let calls = log.iter().filter(|l| l.contains(" call ")).count();
    assert_eq!(calls, 2, "linear code opens exactly two pieces:\n{}", log.join("\n"));
}

#[test]
fn while_true_with_marks_compiles() {
    // The old guard rejected every mark inside `while true`; the loop shape
    // was never the problem. A wait-loop panel is a plain loop.
    let src = r#"
schema { n: sum }
fn reduce(e) { store(["n", store.add(1)]); }
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
"#;
    Cluster::new(&["A"], src, false).expect("while-true wait loop compiles");
}

#[test]
fn far_side_write_before_read_comes_home() {
    // `status` is written on the server before anything there reads it, so
    // it isn't live on entry and doesn't ship. It used to land in a server
    // global; the reply carried nothing and the browser's copy became nil.
    let src = r#"
fn go() {
  let status = "pending";
  server!();
  status = "saved";
  browser!();
  print("status=" .. status);
}

fn go_branch(c) {
  server!();
  let note = "none";
  if c {
    browser!();
    note = "from browser";
  }
  print("note=" .. note);
}
"#;
    expect(&run(src, "go", vec![]), &["[browser A] status=saved"]);
    expect(&run(src, "go_branch", vec![Value::Bool(true)]), &["[server] note=from browser"]);
}
