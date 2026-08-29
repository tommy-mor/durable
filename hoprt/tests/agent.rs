//! Effects and the agent app, offline: bash runs for real (the commands
//! are `echo`), the llm is the harness's deterministic fake ("RUN: <cmd>"
//! yields a bash tool call, anything else echoes, streamed in two
//! chunks). The scenario drives the whole loop — send, stream, parse,
//! approval gate, tool, resume — and ends with replay verification.

use hoprt::harness::Cluster;
use hoprt::value::Value;

fn s(x: &str) -> Value {
    Value::str(x)
}

fn arr(items: Vec<Value>) -> Value {
    Value::array(items)
}

#[test]
fn bash_effect_suspends_and_replies() {
    let src = r#"
        fn go() {
          server!();
          let r = bash("printf hop_out; printf hop_err 1>&2; exit 3");
          print("ok=" .. tostring(r.ok) .. " status=" .. r.status
                .. " out=" .. r.stdout .. " err=" .. r.stderr);
        }
    "#;
    let mut host = Cluster::new(&["A"], src, false).expect("cluster");
    host.fire("A", "go");
    host.pump();
    host.assert_quiescent();
    let all = host.log().join("\n");
    assert!(all.contains("ok=false status=3 out=hop_out err=hop_err"), "{all}");
}

#[test]
fn effects_are_server_side_only() {
    let src = r#"fn go() { bash("echo nope"); }"#;
    let mut host = Cluster::new(&["A"], src, false).expect("cluster");
    host.fire("A", "go");
    host.pump();
    let all = host.log().join("\n");
    assert!(all.contains("server-side only"), "{all}");
    assert!(!all.contains("nope\n"), "{all}");
}

#[test]
fn agent_chat_tool_loop_and_replay() {
    let mut host =
        Cluster::new(&["A"], include_str!("../hop/agent.hop"), false).expect("cluster");
    host.fire_args("server", "on_connect", vec![s("A")]);
    host.pump();

    // a plain question: fake model streams "echo: hello agent" in chunks
    host.set_dom("A", "#draft", "hello agent");
    host.fire("A", "send");
    host.pump();
    host.assert_quiescent();

    let m0 = host.store_get(arr(vec![s("msgs"), Value::Int(0)])).unwrap();
    assert_eq!(m0.get_field("role"), s("user"));
    assert_eq!(m0.get_field("text"), s("hello agent"));
    let m1 = host.store_get(arr(vec![s("msgs"), Value::Int(1)])).unwrap();
    assert_eq!(m1.get_field("role"), s("assistant"));
    assert_eq!(m1.get_field("text"), s("echo: hello agent"));

    // streaming painted partials into #stream before the turn committed
    let streamed = host.dom("A", "#stream");
    assert!(streamed.starts_with("assistant: "), "{streamed}");

    // a tool turn: fake model answers with a bash tool call → approval gate
    host.set_dom("A", "#draft", "RUN: echo hop_tool_ok");
    host.fire("A", "send");
    host.pump();
    host.assert_quiescent();
    let pending = host.store_get(arr(vec![s("pending")])).unwrap();
    assert_eq!(pending, s("echo hop_tool_ok"), "approval gate armed");
    assert!(host.dom("A", "#app").contains("run `echo hop_tool_ok`"), "gate rendered");

    // approve: bash runs (for real), result goes on the tape, turn resumes
    host.fire("A", "approve");
    host.pump();
    host.assert_quiescent();
    let pending = host.store_get(arr(vec![s("pending")])).unwrap();
    assert_eq!(pending, Value::Nil, "gate cleared");
    // msgs: 0 user, 1 assistant, 2 user, 3 tool_request, 4 tool, 5 assistant
    let m4 = host.store_get(arr(vec![s("msgs"), Value::Int(4)])).unwrap();
    assert_eq!(m4.get_field("role"), s("tool"));
    assert!(
        hoprt::value::coerce_str(&m4.get_field("text")).contains("hop_tool_ok"),
        "tool output on the tape: {m4}"
    );
    // and the model saw the output (fake echoes it back)
    let m5 = host.store_get(arr(vec![s("msgs"), Value::Int(5)])).unwrap();
    assert_eq!(m5.get_field("role"), s("assistant"));
    assert!(host.dom("A", "#app").contains("hop_tool_ok"), "transcript rendered");

    // deny path: arm the gate again, refuse it
    host.set_dom("A", "#draft", "RUN: touch /tmp/hop_denied_marker");
    host.fire("A", "send");
    host.pump();
    host.fire("A", "deny");
    host.pump();
    host.assert_quiescent();
    let pending = host.store_get(arr(vec![s("pending")])).unwrap();
    assert_eq!(pending, Value::Nil, "denied gate cleared");
    assert!(
        host.dom("A", "#app").contains("user declined"),
        "declined note rendered"
    );
    assert!(
        !std::path::Path::new("/tmp/hop_denied_marker").exists(),
        "denied command must not run"
    );

    // the tape is the session: incremental execution == replay from zero
    host.verify().unwrap();
}

#[test]
fn agent_auto_approve_runs_tools_without_the_gate() {
    let mut host =
        Cluster::new(&["A"], include_str!("../hop/agent.hop"), false).expect("cluster");
    host.fire("A", "toggle_auto");
    host.pump();
    assert_eq!(host.store_get(arr(vec![s("auto")])).unwrap(), Value::Bool(true));
    assert!(host.dom("A", "#app").contains("auto-approve: on"), "toggle rendered");

    host.set_dom("A", "#draft", "RUN: echo auto_ok");
    host.fire("A", "send");
    host.pump();
    host.assert_quiescent();

    // no gate: the whole turn ran — request, result, model follow-up
    assert_eq!(host.store_get(arr(vec![s("pending")])).unwrap(), Value::Nil);
    // msgs: 0 user, 1 tool_request, 2 tool, 3 assistant
    let m2 = host.store_get(arr(vec![s("msgs"), Value::Int(2)])).unwrap();
    assert_eq!(m2.get_field("role"), s("tool"));
    assert!(
        hoprt::value::coerce_str(&m2.get_field("text")).contains("auto_ok"),
        "tool ran ungated: {m2}"
    );
    let m3 = host.store_get(arr(vec![s("msgs"), Value::Int(3)])).unwrap();
    assert_eq!(m3.get_field("role"), s("assistant"));

    // toggling back re-arms the gate
    host.fire("A", "toggle_auto");
    host.pump();
    assert_eq!(host.store_get(arr(vec![s("auto")])).unwrap(), Value::Bool(false));
    host.verify().unwrap();
}
