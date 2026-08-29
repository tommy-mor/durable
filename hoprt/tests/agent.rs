//! Effects and the agent app, offline: bash runs for real (the commands
//! are `echo`), the llm is the harness's deterministic fake ("RUN: <cmd>"
//! yields a bash tool call, anything else echoes, streamed in two
//! chunks). The scenario drives the whole loop — send, stream, parse,
//! approval gate, tool, resume — and ends with replay verification.
//! The store is thread-aware: every transcript lives at threads[tid].

use hoprt::harness::Cluster;
use hoprt::value::Value;

fn s(x: &str) -> Value {
    Value::str(x)
}

fn arr(items: Vec<Value>) -> Value {
    Value::array(items)
}

/// threads[tid].msgs[i] for thread 1 — the default thread on_connect mints.
fn msg(host: &mut Cluster, tid: i64, i: i64) -> Value {
    host.store_get(arr(vec![s("threads"), Value::Int(tid), s("msgs"), Value::Int(i)]))
        .unwrap()
}

fn pending(host: &mut Cluster, tid: i64) -> Value {
    host.store_get(arr(vec![s("threads"), Value::Int(tid), s("pending")]))
        .unwrap()
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

    // connecting minted thread 1 and pointed this session at it
    let view = host.store_get(arr(vec![s("views"), s("A")])).unwrap();
    assert_eq!(view, Value::Int(1));

    // a plain question: fake model streams "echo: hello agent" in chunks
    host.set_dom("A", "#draft", "hello agent");
    host.fire_args("A", "send", vec![s("A"), Value::Int(1)]);
    host.pump();
    host.assert_quiescent();

    let m0 = msg(&mut host, 1, 0);
    assert_eq!(m0.get_field("role"), s("user"));
    assert_eq!(m0.get_field("text"), s("hello agent"));
    let m1 = msg(&mut host, 1, 1);
    assert_eq!(m1.get_field("role"), s("assistant"));
    assert_eq!(m1.get_field("text"), s("echo: hello agent"));

    // the first message became the thread's title
    let title = host
        .store_get(arr(vec![s("threads"), Value::Int(1), s("title")]))
        .unwrap();
    assert_eq!(title, s("hello agent"));

    // streaming painted partials into #stream before the turn committed
    let streamed = host.dom("A", "#stream");
    assert!(streamed.starts_with("assistant: "), "{streamed}");

    // a tool turn: fake model answers with a bash tool call → approval gate
    host.set_dom("A", "#draft", "RUN: echo hop_tool_ok");
    host.fire_args("A", "send", vec![s("A"), Value::Int(1)]);
    host.pump();
    host.assert_quiescent();
    assert_eq!(pending(&mut host, 1), s("echo hop_tool_ok"), "approval gate armed");
    assert!(host.dom("A", "#app").contains("run `echo hop_tool_ok`"), "gate rendered");

    // approve: bash runs (for real), result goes on the tape, turn resumes
    host.fire_args("A", "approve", vec![Value::Int(1)]);
    host.pump();
    host.assert_quiescent();
    assert_eq!(pending(&mut host, 1), Value::Nil, "gate cleared");
    // msgs: 0 user, 1 assistant, 2 user, 3 tool_request, 4 tool, 5 assistant
    let m4 = msg(&mut host, 1, 4);
    assert_eq!(m4.get_field("role"), s("tool"));
    assert!(
        hoprt::value::coerce_str(&m4.get_field("text")).contains("hop_tool_ok"),
        "tool output on the tape: {m4}"
    );
    // and the model saw the output (fake echoes it back)
    let m5 = msg(&mut host, 1, 5);
    assert_eq!(m5.get_field("role"), s("assistant"));
    assert!(host.dom("A", "#app").contains("hop_tool_ok"), "transcript rendered");

    // deny path: arm the gate again, refuse it
    host.set_dom("A", "#draft", "RUN: touch /tmp/hop_denied_marker");
    host.fire_args("A", "send", vec![s("A"), Value::Int(1)]);
    host.pump();
    host.fire_args("A", "deny", vec![Value::Int(1)]);
    host.pump();
    host.assert_quiescent();
    assert_eq!(pending(&mut host, 1), Value::Nil, "denied gate cleared");
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
fn agent_threads_are_independent_and_sessions_view_their_own() {
    let mut host =
        Cluster::new(&["A", "B"], include_str!("../hop/agent.hop"), false).expect("cluster");
    host.fire_args("server", "on_connect", vec![s("A")]);
    host.fire_args("server", "on_connect", vec![s("B")]);
    host.pump();

    // both tabs land on thread 1; B forks its own
    host.fire_args("B", "new_thread", vec![s("B")]);
    host.pump();
    assert_eq!(host.store_get(arr(vec![s("views"), s("A")])).unwrap(), Value::Int(1));
    assert_eq!(host.store_get(arr(vec![s("views"), s("B")])).unwrap(), Value::Int(2));

    // each session talks in its own thread
    host.set_dom("A", "#draft", "apples");
    host.fire_args("A", "send", vec![s("A"), Value::Int(1)]);
    host.set_dom("B", "#draft", "bananas");
    host.fire_args("B", "send", vec![s("B"), Value::Int(2)]);
    host.pump();
    host.assert_quiescent();

    // transcripts did not bleed into each other
    assert_eq!(msg(&mut host, 1, 0).get_field("text"), s("apples"));
    assert_eq!(msg(&mut host, 1, 1).get_field("text"), s("echo: apples"));
    assert_eq!(msg(&mut host, 2, 0).get_field("text"), s("bananas"));
    assert_eq!(msg(&mut host, 2, 1).get_field("text"), s("echo: bananas"));

    // each tab renders its own transcript (titles show in both tab lists,
    // so test on the assistant replies, which only transcripts contain)
    assert!(host.dom("A", "#app").contains("echo: apples"), "A sees thread 1");
    assert!(!host.dom("A", "#app").contains("echo: bananas"), "A's transcript is thread 1 only");
    assert!(host.dom("B", "#app").contains("echo: bananas"), "B sees thread 2");
    assert!(host.dom("A", "#app").contains("bananas"), "thread 2 titled in A's tab list");

    // A hops over to B's thread and sees it
    host.fire_args("A", "open_thread", vec![s("A"), Value::Int(2)]);
    host.pump();
    assert_eq!(host.store_get(arr(vec![s("views"), s("A")])).unwrap(), Value::Int(2));
    assert!(host.dom("A", "#app").contains("echo: bananas"), "A now sees thread 2");

    host.verify().unwrap();
}

#[test]
fn agent_model_picker_and_home_dir() {
    let mut host =
        Cluster::new(&["A"], include_str!("../hop/agent.hop"), false).expect("cluster");
    host.fire_args("server", "on_connect", vec![s("A")]);
    host.pump();
    host.assert_quiescent();

    // on_connect fetched the provider catalog (the harness's fake list)
    let models = host.store_get(arr(vec![s("models")])).unwrap();
    assert_eq!(models, arr(vec![s("fake/alpha"), s("fake/beta")]));
    assert!(host.dom("A", "#app").contains("fake/alpha"), "datalist rendered");

    // pick a model; it lands in the store (and thus in every llm req)
    host.set_dom("A", "#model_pick", "fake/beta");
    host.fire("A", "set_model");
    host.pump();
    assert_eq!(host.store_get(arr(vec![s("model")])).unwrap(), s("fake/beta"));

    // set the working directory; bash runs there
    host.set_dom("A", "#home_pick", "/tmp");
    host.fire("A", "set_home");
    host.pump();
    assert_eq!(host.store_get(arr(vec![s("home")])).unwrap(), s("/tmp"));

    host.fire("A", "toggle_auto");
    host.pump();
    host.set_dom("A", "#draft", "RUN: pwd");
    host.fire_args("A", "send", vec![s("A"), Value::Int(1)]);
    host.pump();
    host.assert_quiescent();
    // msgs: 0 user, 1 tool_request, 2 tool, 3 assistant
    let m2 = msg(&mut host, 1, 2);
    assert!(
        hoprt::value::coerce_str(&m2.get_field("text")).contains("tmp"),
        "pwd ran in the configured dir: {m2}"
    );

    // an empty value clears back to the default
    host.set_dom("A", "#model_pick", "");
    host.fire("A", "set_model");
    host.pump();
    assert_eq!(host.store_get(arr(vec![s("model")])).unwrap(), Value::Nil);

    host.verify().unwrap();
}

#[test]
fn agent_auto_approve_runs_tools_without_the_gate() {
    let mut host =
        Cluster::new(&["A"], include_str!("../hop/agent.hop"), false).expect("cluster");
    host.fire_args("server", "on_connect", vec![s("A")]);
    host.pump();
    host.fire("A", "toggle_auto");
    host.pump();
    assert_eq!(host.store_get(arr(vec![s("auto")])).unwrap(), Value::Bool(true));
    assert!(host.dom("A", "#app").contains("auto-approve: on"), "toggle rendered");

    host.set_dom("A", "#draft", "RUN: echo auto_ok");
    host.fire_args("A", "send", vec![s("A"), Value::Int(1)]);
    host.pump();
    host.assert_quiescent();

    // no gate: the whole turn ran — request, result, model follow-up
    assert_eq!(pending(&mut host, 1), Value::Nil);
    // msgs: 0 user, 1 tool_request, 2 tool, 3 assistant
    let m2 = msg(&mut host, 1, 2);
    assert_eq!(m2.get_field("role"), s("tool"));
    assert!(
        hoprt::value::coerce_str(&m2.get_field("text")).contains("auto_ok"),
        "tool ran ungated: {m2}"
    );
    let m3 = msg(&mut host, 1, 3);
    assert_eq!(m3.get_field("role"), s("assistant"));

    // toggling back re-arms the gate
    host.fire("A", "toggle_auto");
    host.pump();
    assert_eq!(host.store_get(arr(vec![s("auto")])).unwrap(), Value::Bool(false));
    host.verify().unwrap();
}
