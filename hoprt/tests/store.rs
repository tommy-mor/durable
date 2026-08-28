//! Durable store: hop todo events land on the JSONL tape, survive reopen,
//! and incremental execution equals replay from zero.

use hoprt::compiler;
use hoprt::harness::Host;

fn todo_code() -> String {
    let src = include_str!("../hop/todo.hop");
    let lua = compiler::compile(src).expect("hopc compile");
    let dom_stub =
        "dom = { set = function(sel, html) print(\"[dom] \" .. sel .. \" := \" .. html) end }\n";
    format!("{dom_stub}{lua}")
}

#[test]
fn todo_persists_across_reopen_and_verifies() {
    let code = todo_code();
    let dir = tempfile::tempdir().unwrap();

    {
        let host = Host::with_data_dir(&["A"], &code, false, dir.path()).expect("host");
        host.fire("A", "sim_demo").unwrap();
        host.pump().unwrap();
        host.assert_quiescent().unwrap();

        let applied: u64 = host.eval_server("return store.applied()").unwrap();
        assert_eq!(applied, 2);

        let milk: String = host
            .eval_server(r#"return store.one({"todos", 0, "text"})"#)
            .unwrap();
        assert_eq!(milk, "buy milk");

        host.eval_server::<()>("store.verify()").unwrap();
    }

    // New process, same tape: catch-up restores the projection.
    let host = Host::with_data_dir(&["A"], &code, false, dir.path()).expect("reopen");
    let applied: u64 = host.eval_server("return store.applied()").unwrap();
    assert_eq!(applied, 2);
    let milk: String = host
        .eval_server(r#"return store.one({"todos", 0, "text"})"#)
        .unwrap();
    assert_eq!(milk, "buy milk");
    let compiler: String = host
        .eval_server(r#"return store.one({"todos", 1, "text"})"#)
        .unwrap();
    assert_eq!(compiler, "write the compiler");
    host.eval_server::<()>("store.verify()").unwrap();

    let log = std::fs::read_to_string(dir.path().join("log.jsonl")).unwrap();
    assert!(log.contains(r#""type":"add""#), "{log}");
    assert_eq!(log.lines().filter(|l| !l.trim().is_empty()).count(), 2);
}

#[test]
fn rebuild_from_tape_matches_live() {
    let code = todo_code();
    let host = Host::new(&["A"], &code, false).expect("host");
    host.fire("A", "sim_demo").unwrap();
    host.pump().unwrap();
    host.eval_server::<()>("store.rebuild()").unwrap();
    host.eval_server::<()>("store.verify()").unwrap();
    let created: i64 = host
        .eval_server(r#"return store.one({"stats", "created"})"#)
        .unwrap();
    assert_eq!(created, 2);
}
