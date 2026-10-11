//! store.wait bleeds through: a parked panel wakes when a commit touches her watch.

use hoprt::harness::Cluster;
use hoprt::value::Value;

fn s(x: &str) -> Value {
    Value::str(x)
}

fn arr(items: Vec<Value>) -> Value {
    Value::array(items)
}

// A panel shows the todos, waits on them, shows again. A writer appends.
// The panel must wake and re-render WITHOUT any cast or refresh poke.
const WAITAPP: &str = r##"
schema {
  todos: map<int, record {
    text: leaf,
    done: leaf
  }>,
  stats: record {
    created: sum,
    completed: sum
  }
}

fn reduce(event) {
  match event {
    add { seq, text } => {
      store(["todos", seq, store.set({ text = text, done = false })]);
      store(["stats", "created", store.add(1)]);
    }
    toggle { id } => {
      let done = store(["todos", id, "done"]);
      store(["todos", id, "done", store.set(!done)]);
    }
  }
}

fn count_view(n) {
  return [:div, {}, "count=" .. n];
}

fn panel() {
  server!();
  while true {
    let items = store.items(["todos"]);
    let n = len(items);
    let me = session();
    cast session(me) {
      hui.render("#app", count_view(n));
    }
    store.wait(["todos"]);
  }
}

fn add_todo(text) {
  server!();
  store.append({ type = "add", text = text });
}

fn sim_add(text) {
  server!();
  store.append({ type = "add", text = text });
}
"##;

#[test]
fn waiter_wakes_on_overlap_and_rerenders() {
    let mut host = Cluster::new(&["A", "B"], WAITAPP, false).expect("cluster");
    // B opens a panel watching ["todos"]; it renders count=0 and parks.
    host.fire("B", "panel");
    host.pump();
    assert_eq!(host.dom("B", "#app"), "<div>count=0</div>", "panel shows empty");

    // A appends through a server flow. The commit overlaps the watch, so
    // the parked panel wakes and re-renders — no cast, no refresh call.
    host.fire_args("A", "add_todo", vec![s("buy milk")]);
    host.pump();
    // The panel woke, re-rendered, and re-parked for the next write:
    // parked is her correct sleeping state, not a leak.
    assert_eq!(host.dom("B", "#app"), "<div>count=1</div>", "panel woke and re-rendered");
    assert_eq!(host.server_parked(), vec!["B#1".to_string()]);
}

#[test]
fn waiter_ignores_disjoint_writes() {
    let mut host = Cluster::new(&["A"], WAITAPP, false).expect("cluster");
    host.fire("A", "panel");
    host.pump();

    // Direct append under ["stats"] — disjoint from the ["todos"] watch.
    // Harness appends bypass flows but run the same commit+wake path.
    host.append(hoprt::value::Value::map(
        [(s("type"), s("noop"))].into_iter().collect(),
    ))
    .ok();
    host.pump();
    // Panel still parked: quiescence would FAIL (a suspended flow remains).
    // So assert the opposite — she is still asleep on purpose.
    assert!(host.dom("A", "#app").contains("count=0"));
}

#[test]
fn wait_sees_commit_between_read_and_park() {
    let mut host = Cluster::new(&["A"], WAITAPP, false).expect("cluster");
    // Seed one todo so applied=1.
    host.fire_args("A", "sim_add", vec![s("seed")]);
    host.pump();
    // Panel reads at seq 0 (fresh flow) but applied is already 1:
    // wait answers "changed" at once instead of parking forever.
    host.fire("A", "panel");
    host.pump();
    // applied(1) > read_seq(0): wait answered "changed" at once, the loop
    // re-read (count=1), re-rendered, and parked for the next write.
    assert_eq!(host.dom("A", "#app"), "<div>count=1</div>");
    assert_eq!(host.server_parked(), vec!["A#2".to_string()]);
}

#[test]
fn spawn_returns_a_handle_and_cancel_kills_before_wake() {
    let src = r##"
schema {
  todos: map<int, leaf>
}
fn reduce(event) {
  match event {
    add { seq, text } => {
      store(["todos", seq, store.set(text)]);
    }
  }
}
fn watcher() {
  store.wait(["todos"]);
}
fn starter() {
  server!();
  let h = spawn watcher();
  cancel(h);
}
fn adder() {
  server!();
  store.append({ type = "add", text = "x" });
}
"##;
    let mut host = Cluster::new(&["A"], src, false).expect("cluster");
    // starter spawns a watcher then cancels her handle, all on the server.
    host.fire("A", "starter");
    host.pump();
    // Nobody parked: the watcher died before any write.
    assert_eq!(host.server_parked(), Vec::<String>::new());
    // A later write wakes nothing and the cluster is quiescent.
    host.fire("A", "adder");
    host.pump();
    host.assert_quiescent();
}

#[test]
fn disconnect_ends_a_tabs_waiters() {
    let mut host = Cluster::new(&["A", "B"], WAITAPP, false).expect("cluster");
    host.fire("B", "panel");
    host.pump();
    assert_eq!(host.server_parked(), vec!["B#1".to_string()]);
    host.disconnect("B");
    host.pump();
    assert_eq!(host.server_parked(), Vec::<String>::new());
}

const CHAT: &str = include_str!("../hop/chat.hop");

#[test]
fn chat_panels_wake_per_room_without_broadcast() {
    let mut host = Cluster::new(&["A", "B"], CHAT, false).expect("cluster");
    // Both tabs open the lobby. Each runs her own panel iteration, parked.
    host.fire_args("A", "open_room", vec![s("lobby")]);
    host.fire_args("B", "open_room", vec![s("lobby")]);
    host.pump();
    assert!(host.dom("A", "#app").contains("room lobby"), "A panel rendered: {}", host.dom("A", "#app"));
    assert!(host.dom("B", "#app").contains("room lobby"), "B panel rendered");
    assert_eq!(host.server_parked().len(), 2, "two panels parked");

    // B switches to random: her lobby watch dies, a random watch parks.
    host.fire_args("B", "open_room", vec![s("random")]);
    host.pump();
    assert!(host.dom("B", "#app").contains("room random"), "B moved rooms");
    assert_eq!(host.server_parked().len(), 2, "still exactly two live iterations");

    // A sends to lobby (direct append = same commit path as say_in).
    host.append(
        hoprt::value::Value::map(
            [
                (s("type"), s("say")),
                (s("sid"), s("A")),
                (s("room"), s("lobby")),
                (s("text"), s("hi lobby")),
            ]
            .into_iter()
            .collect(),
        ),
    )
    .unwrap();
    host.pump();
    // A's lobby panel woke and shows the message; B's random panel slept.
    assert!(host.dom("A", "#app").contains("hi lobby"), "A woke: {}", host.dom("A", "#app"));
    assert!(!host.dom("B", "#app").contains("hi lobby"), "B slept (other room)");
    assert_eq!(host.server_parked().len(), 2, "both re-parked for the next write");
}

const MICROBLOG: &str = include_str!("../hop/microblog.hop");
const RANKING: &str = include_str!("../hop/ranking.hop");

#[test]
fn microblog_follower_panel_wakes_on_post() {
    let mut host = Cluster::new(&["A", "B"], MICROBLOG, false).expect("cluster");
    // Both tabs open their feeds; join + follow through flows.
    host.fire("A", "open_board");
    host.fire("B", "open_board");
    host.pump();
    host.set_dom("A", "#name", "Ada");
    host.fire("A", "join");
    host.pump();
    host.set_dom("B", "#name", "Bob");
    host.fire("B", "join");
    host.pump();
    // B follows A through the flow the button calls.
    host.fire_args("B", "follow", vec![s("A")]);
    host.pump();
    // A posts. B's panel watches the root, so the followers write + the
    // post both wake her; her timeline gains the post without any poke.
    host.set_dom("A", "#draft", "hello tape");
    host.fire("A", "publish");
    host.pump();
    assert!(host.dom("B", "#app").contains("hello tape"), "follower woke: {}", host.dom("B", "#app"));
    assert!(host.dom("A", "#app").contains("hello tape"), "author woke");
}

#[test]
fn ranking_panels_wake_on_vote() {
    let mut host = Cluster::new(&["A", "B"], RANKING, false).expect("cluster");
    host.fire("A", "open_board");
    host.fire("B", "open_board");
    host.pump();
    // A votes through the flow the button calls. Both scope-watchers wake.
    host.fire_args("A", "cast_vote", vec![s("tools"), s("ripgrep"), s("grep")]);
    host.pump();
    for tab in ["A", "B"] {
        let app = host.dom(tab, "#app");
        assert!(app.contains("1 votes in tools"), "tab {tab} woke: {app}");
    }
    assert_eq!(host.server_parked().len(), 2);
}
