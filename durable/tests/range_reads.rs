//! End-to-end tests covering every use case from `RANGE_READS_WISHLIST.md`
//! (modeled directly on `react-sorter`).

use durable::{
    Dir, Durable, Leaf, Map, OrderedF64, Reverse, Runtime, Set, Sum, Tx,
};
use serde::{Deserialize, Serialize};
use tempfile::TempDir;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
struct Vote {
    id: i64,
    user: String,
    item_id: i64,
    ts_ms: i64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
struct Item {
    id: i64,
    title: String,
    score: f64,
}

#[derive(Durable)]
#[allow(dead_code)]
struct SorterStore {
    users: Map<String, Leaf<String>>,
    items: Map<i64, Leaf<Item>>,
    votes: Map<i64, Leaf<Vote>>,
    /// Use case 1 & 2: user -> set of vote ids (newest-first + stable cursor paging)
    user_votes: Map<String, Set<i64>>,
    /// Use case 3: item -> set of vote ids (recent activity per item)
    item_votes: Map<i64, Set<i64>>,
    /// Use case 7: composite key `(provider, subject) -> user`
    identities: Map<(String, String), Leaf<String>>,
    /// Use case 8: time-ordered secondary index `(ts_ms, vote_id)`
    votes_by_time: Set<(i64, i64)>,
    /// Bonus: on-disk leaderboard `(Reverse<OrderedF64>, item_id)`
    leaderboard: Set<(Reverse<OrderedF64>, i64)>,
    /// Optional O(1) per-user vote count via Sum
    user_vote_counts: Map<String, Sum<u64>>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
enum SorterEvent {
    AddUser { name: String, bio: String },
    AddIdentity { provider: String, subject: String, user: String },
    AddItem { id: i64, title: String, score: f64 },
    UpdateScore { id: i64, new_score: f64 },
    CastVote { id: i64, user: String, item_id: i64, ts_ms: i64 },
}

fn reduce(tx: &mut Tx, ev: &SorterEvent) -> durable::Result<()> {
    let root = SorterStore::root();
    match ev {
        SorterEvent::AddUser { name, bio } => {
            root.users().set(tx, name, bio)?;
        }
        SorterEvent::AddIdentity {
            provider,
            subject,
            user,
        } => {
            root.identities()
                .set(tx, (provider.as_str(), subject.as_str()), user)?;
        }
        SorterEvent::AddItem { id, title, score } => {
            root.items().set(
                tx,
                *id,
                &Item {
                    id: *id,
                    title: title.clone(),
                    score: *score,
                },
            )?;
            root.leaderboard()
                .insert(tx, (Reverse(OrderedF64(*score)), *id))?;
        }
        SorterEvent::UpdateScore { id, new_score } => {
            if let Some(mut item) = root.items().get(tx, *id)? {
                root.leaderboard()
                    .remove(tx, (Reverse(OrderedF64(item.score)), *id))?;
                item.score = *new_score;
                root.items().set(tx, *id, &item)?;
                root.leaderboard()
                    .insert(tx, (Reverse(OrderedF64(*new_score)), *id))?;
            }
        }
        SorterEvent::CastVote {
            id,
            user,
            item_id,
            ts_ms,
        } => {
            let vote = Vote {
                id: *id,
                user: user.clone(),
                item_id: *item_id,
                ts_ms: *ts_ms,
            };
            root.votes().set(tx, *id, &vote)?;
            root.user_votes().key(user).insert(tx, *id)?;
            root.item_votes().key(*item_id).insert(tx, *id)?;
            root.votes_by_time().insert(tx, (*ts_ms, *id))?;
            root.user_vote_counts().add(tx, user, 1)?;
        }
    }
    Ok(())
}

#[test]
fn all_eight_react_sorter_wishlist_use_cases() {
    let dir = TempDir::new().unwrap();
    let rt = Runtime::open::<SorterStore>(
        dir.path().join("proj"),
        dir.path().join("tape.jsonl"),
        reduce,
    )
    .unwrap();

    // Seed users, identities, items, and 2,500 votes (batched for speed).
    let mut batch = Vec::new();
    for name in [
        "alice", "bob", "tom", "tommy", "tomas", "tony", "zara", "aaa",
    ] {
        batch.push(SorterEvent::AddUser {
            name: name.into(),
            bio: format!("bio of {name}"),
        });
    }
    for (prov, sub, u) in [
        ("github", "101", "tommy"),
        ("github", "202", "alice"),
        ("github", "303", "bob"),
        ("google", "999", "zara"),
    ] {
        batch.push(SorterEvent::AddIdentity {
            provider: prov.into(),
            subject: sub.into(),
            user: u.into(),
        });
    }
    for (id, title, score) in [
        (1, "Rust", 98.5),
        (2, "Go", 80.0),
        (3, "Zig", 91.2),
        (4, "C++", 75.0),
    ] {
        batch.push(SorterEvent::AddItem {
            id,
            title: title.into(),
            score,
        });
    }
    rt.append_batch(batch).unwrap();

    // 2,500 votes across 3 batches (including negative timestamps/ids to verify signed order)
    for chunk in 0..5 {
        let mut votes = Vec::new();
        for i in 0..500i64 {
            let vid = chunk * 500 + i;
            let user = if vid % 2 == 0 { "tommy" } else { "alice" };
            let item_id = (vid % 4) + 1;
            let ts_ms = 1_700_000_000_000 + vid * 1000;
            votes.push(SorterEvent::CastVote {
                id: vid,
                user: user.into(),
                item_id,
                ts_ms,
            });
        }
        rt.append_batch(votes).unwrap();
    }

    let root = SorterStore::root();
    let r = rt.read().unwrap();

    // Use case 1: Newest-first pagination of an index (`user_votes["tommy"]`, 50 per page)
    let (page1_ids, cursor1) = root
        .user_votes()
        .key("tommy")
        .page(&r, None::<i64>, 50, Dir::Desc)
        .unwrap();
    assert_eq!(page1_ids.len(), 50);
    assert_eq!(page1_ids[0], 2498);
    assert_eq!(page1_ids[49], 2400);
    assert_eq!(cursor1, Some(2400));

    // Hydrate page 1 votes in the SAME snapshot with get_many
    let hydrated = root.votes().get_many(&r, page1_ids.iter().copied()).unwrap();
    assert_eq!(hydrated.len(), 50);
    assert_eq!(hydrated[0].as_ref().unwrap().id, 2498);

    // Use case 2: Cursor pagination is stable under new appends!
    rt.append(SorterEvent::CastVote {
        id: 9999,
        user: "tommy".into(),
        item_id: 1,
        ts_ms: 1_700_009_999_000,
    })
    .unwrap();
    let r2 = rt.read().unwrap();
    let (page2_ids, cursor2) = root
        .user_votes()
        .key("tommy")
        .page(&r2, cursor1, 50, Dir::Desc)
        .unwrap();
    assert_eq!(page2_ids.len(), 50);
    assert_eq!(page2_ids[0], 2398, "new append did not shift page 2!");
    assert_eq!(cursor2, Some(2300));

    // Use case 3: Recent activity per item & global recent votes feed
    let recent_item1: Vec<i64> = root
        .item_votes()
        .key(1)
        .range(&r2, ..)
        .unwrap()
        .rev()
        .take(20)
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(recent_item1.len(), 20);
    assert_eq!(recent_item1[0], 9999);
    assert_eq!(recent_item1[1], 2496);

    let recent_global: Vec<(i64, Vote)> = root
        .votes()
        .range(&r2, ..)
        .unwrap()
        .rev()
        .take(5)
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(recent_global.len(), 5);
    assert_eq!(recent_global[0].0, 9999);

    // Use case 4: O(1) counts on root tables without decoding!
    assert_eq!(root.users().len(&r2).unwrap(), 8);
    assert_eq!(root.items().len(&r2).unwrap(), 4);
    assert_eq!(root.votes().len(&r2).unwrap(), 2501);
    assert_eq!(root.user_vote_counts().get(&r2, "tommy").unwrap(), 1251);

    // Use case 5: O(1)/1-seek existence checks
    assert!(root.users().contains(&r2, "tommy").unwrap());
    assert!(!root.users().contains(&r2, "nobody").unwrap());
    assert!(!root.votes().is_empty(&r2).unwrap());

    // Use case 6: Lexicographic prefix search on string keys ("tom", limit 10)
    let matches: Vec<String> = root
        .users()
        .prefix_keys(&r2, "tom")
        .unwrap()
        .take(10)
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(matches, vec!["tom", "tomas", "tommy"]);
    // Confirm lexicographic order over all users ("aaa" before "alice", "tony" before "zara")
    assert_eq!(root.users().first(&r2).unwrap(), Some("aaa".to_string()));
    assert_eq!(root.users().last(&r2).unwrap(), Some("zara".to_string()));

    // Use case 7: Range / prefix over first element of tuple key
    let github_ids: Vec<((String, String), String)> = root
        .identities()
        .prefix(&r2, ("github",))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        github_ids,
        vec![
            (("github".into(), "101".into()), "tommy".into()),
            (("github".into(), "202".into()), "alice".into()),
            (("github".into(), "303".into()), "bob".into()),
        ]
    );

    // Use case 8: Time-ordered secondary index range query `[t0, t1)`
    let t0 = 1_700_000_010_000i64;
    let t1 = 1_700_000_015_000i64;
    let window: Vec<(i64, i64)> = root
        .votes_by_time()
        .range(&r2, (t0, i64::MIN)..(t1, i64::MIN))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(window.len(), 5);
    assert_eq!(window[0].1, 10);
    assert_eq!(window[4].1, 14);

    // Bonus: On-disk leaderboard (highest score first via Reverse<OrderedF64>)
    rt.append(SorterEvent::UpdateScore {
        id: 2,
        new_score: 99.9,
    })
    .unwrap();
    let r3 = rt.read().unwrap();
    let top3: Vec<i64> = root
        .leaderboard()
        .range(&r3, ..)
        .unwrap()
        .take(3)
        .map(|res| res.map(|(_, id)| id))
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(top3, vec![2, 1, 3]); // Go (99.9), Rust (98.5), Zig (91.2)

    rt.verify().unwrap();
}
