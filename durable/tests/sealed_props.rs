//! Property hammer for the sealed scan: batch shapes, contents, cut points,
//! garble spans. The committed prefix must stand under every assault.

use durable::runtime::{render_sealed, scan};
use proptest::prelude::*;

fn seal_event(seq: u64, from: u64, to: u64, note: &str) -> String {
    let event = serde_json::json!({"note": note});
    render_sealed(seq, 1000 + seq, from, to, &event).unwrap()
}

/// A random committed tape: 0–5 batches, 1–4 records each, arbitrary notes.
fn arb_tape() -> impl Strategy<Value = Vec<u8>> {
    proptest::collection::vec(
        proptest::collection::vec("[a-z0-9 ]{0,24}", 1..4),
        0..5,
    )
    .prop_map(|batches| {
        let mut out = Vec::new();
        let mut seq = 0u64;
        for notes in batches {
            let from = seq;
            let to = seq + notes.len() as u64 - 1;
            for note in notes {
                out.extend_from_slice(seal_event(seq, from, to, &note).as_bytes());
                out.push(b'\n');
                seq += 1;
            }
        }
        out
    })
}

proptest! {
    /// Any truncation of any tape reopens to a prefix: committed len only
    /// shrinks to a batch boundary, end matches the surviving bytes.
    #[test]
    fn truncation_never_invents_or_reorders(tape in arb_tape(), cut in any::<usize>()) {
        let full = scan(&tape).expect("generated tape is clean");
        prop_assert_eq!(full.end, tape.len());
        let at = cut % (tape.len() + 1);
        let found = scan(&tape[..at]).expect("truncation always scans clean");
        prop_assert!(found.len <= full.len, "cut {at}: {} > {}", found.len, full.len);
        prop_assert!(found.end <= at, "cut {at}: end {} past cut", found.end);
        // Committed end is always a line boundary (or zero).
        if found.end > 0 {
            prop_assert_eq!(tape[found.end - 1], b'\n');
        }
    }

    /// Garbling any span inside the FINAL batch keeps boot clean: the
    /// committed prefix stands, never a fatal error (no later batch
    /// survives to prove damage to acknowledged bytes).
    #[test]
    fn garble_in_final_batch_drops_not_dies(
        mut tape in arb_tape(),
        span in 1usize..64,
    ) {
        if tape.is_empty() {
            return Ok(());
        }
        let full = scan(&tape).expect("generated tape is clean");
        // Garble the last span bytes (inside the final batch or its tail).
        let n = span % tape.len().max(1);
        let from = tape.len() - n;
        for b in &mut tape[from..] {
            if *b != b'\n' {
                *b = b'X';
            }
        }
        let found = scan(&tape).expect("final-batch garble must not be fatal");
        prop_assert!(found.len <= full.len);
    }

    /// Flipping any single non-newline byte either keeps the tape clean
    /// (CRC still valid is impossible — so: drops tail or dies fatal),
    /// but NEVER invents records.
    #[test]
    fn single_flip_never_invents(tape in arb_tape(), pos in any::<usize>()) {
        if tape.is_empty() {
            return Ok(());
        }
        let full = scan(&tape).expect("generated tape is clean");
        let mut cut = tape.clone();
        let i = pos % cut.len();
        if cut[i] == b'\n' {
            return Ok(());
        }
        cut[i] = if cut[i] == b'0' { b'1' } else { b'0' };
        match scan(&cut) {
            Ok(found) => prop_assert!(found.len <= full.len, "flip {i} invented records"),
            Err(_) => {}
        }
    }
}

/// Multi-batch reopen continuity: commits, torn residue, reopen, more
/// commits — applied only ever grows along batch boundaries.
#[test]
fn reopen_continuity_across_batches() {
    use durable::{Durable, Leaf, Map, Runtime};
    use serde::{Deserialize, Serialize};
    use tempfile::TempDir;

    #[derive(Durable)]
    #[allow(dead_code)]
    struct Store {
        seen: Map<String, Leaf<String>>,
    }
    #[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
    struct Ev {
        note: String,
    }
    fn reduce(tx: &mut durable::Tx, e: &Ev) -> durable::Result<()> {
        tx.write(Store::root().seen().key(&e.note).set(&e.note));
        Ok(())
    }
    let dir = TempDir::new().unwrap();
    let log = dir.path().join("log.jsonl");
    let proj = dir.path().join("proj");
    // Three multi-event batches, reopening between each.
    for round in 0..3 {
        {
            let rt = Runtime::open_described::<Store>(&proj, &log, None, reduce).unwrap();
            let evs: Vec<Ev> = (0..3).map(|i| Ev { note: format!("r{round}i{i}") }).collect();
            rt.append_batch(evs).unwrap();
        }
        // Drool torn residue past the end, then reopen: she rises with
        // the prefix and the next batch lands contiguous.
        let mut bytes = std::fs::read(&log).unwrap();
        bytes.extend_from_slice(b"{\"seq\":999,\"ts_ms\":1");
        std::fs::write(&log, &bytes).unwrap();
    }
    let rt = Runtime::open_described::<Store>(&proj, &log, None, reduce).unwrap();
    assert_eq!(rt.applied().unwrap(), 9);
    rt.verify().unwrap();
}
