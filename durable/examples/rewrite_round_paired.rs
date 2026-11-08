//! Worked example: evolving a tape by an offline rewrite.
//!
//! The old tape has `RoundBegan { round, map }`. Its meaning depends on who was
//! waiting: the old fold pairs the waiting players itself. The new rule records
//! the decision as data: `RoundPaired { round, map, flushed, arenas }`. The
//! arenas are computed from the state BEFORE the old record is folded, using
//! the OLD rule, so the rewritten tape folds to exactly the same projection.
//!
//! Steps, as an operator runs them (see docs/evolving-a-tape.md):
//!   1. snapshot the source tape (this program works on a copy in a temp dir);
//!   2. dry-run, to see how many records a rewrite would replace;
//!   3. rewrite to a NEW file (never in place);
//!   4. prove equivalence: old tape + old fold == new tape + new fold;
//!   5. rename over the tape with the writer stopped (printed, not done here).
//!
//! Run: `cargo run -p durable --example rewrite_round_paired`

use std::path::{Path, PathBuf};

use durable::testing::{app_keys_only, check_rewrite_equivalent};
use durable::Describe;
use durable::{dry_run, rewrite, Durable, Emit, Leaf, Mode, Result, Runtime, Shape, Step, Tx};
use serde::{Deserialize, Serialize};

/// The projection both rules write.
#[derive(Durable)]
pub struct Ladder {
    /// Players waiting for the next round, in arrival order.
    pub waiting: Leaf<Vec<String>>,
    /// The arenas of the most recent round, as "a vs b" strings.
    pub last_arenas: Leaf<Vec<String>>,
    /// Players flushed out of the waiting list by the most recent round.
    pub last_flushed: Leaf<Vec<String>>,
}

/// The OLD event vocabulary, as it sits on the tape today.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Old {
    Joined { player: String },
    RoundBegan { round: u64, map: String },
}

/// The NEW vocabulary: the decision is recorded in the event.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum New {
    Joined {
        player: String,
    },
    RoundPaired {
        round: u64,
        map: String,
        flushed: Vec<String>,
        arenas: Vec<String>,
    },
}

/// The old rule: pair waiting players in arrival order, two per arena; the
/// odd one out stays waiting. Returns (arenas, flushed).
pub fn legacy_pairing(waiting: &[String]) -> (Vec<String>, Vec<String>) {
    let mut arenas = Vec::new();
    let mut flushed = Vec::new();
    for pair in waiting.chunks(2) {
        if let [a, b] = pair {
            arenas.push(format!("{a} vs {b}"));
            flushed.push(a.clone());
            flushed.push(b.clone());
        }
    }
    (arenas, flushed)
}

pub fn reduce_old(tx: &mut Tx, ev: &Old) -> Result<()> {
    let root = Ladder::root();
    match ev {
        Old::Joined { player } => {
            let mut waiting = root.waiting().get(tx.db())?.unwrap_or_default();
            waiting.push(player.clone());
            tx.write(root.waiting().set(&waiting));
        }
        Old::RoundBegan { .. } => {
            let waiting = root.waiting().get(tx.db())?.unwrap_or_default();
            let (arenas, flushed) = legacy_pairing(&waiting);
            let rest: Vec<String> = waiting
                .into_iter()
                .filter(|p| !flushed.contains(p))
                .collect();
            tx.write(root.waiting().set(&rest));
            tx.write(root.last_arenas().set(&arenas));
            tx.write(root.last_flushed().set(&flushed));
        }
    }
    Ok(())
}

pub fn reduce_new(tx: &mut Tx, ev: &New) -> Result<()> {
    let root = Ladder::root();
    match ev {
        New::Joined { player } => {
            let mut waiting = root.waiting().get(tx.db())?.unwrap_or_default();
            waiting.push(player.clone());
            tx.write(root.waiting().set(&waiting));
        }
        New::RoundPaired {
            flushed, arenas, ..
        } => {
            // No pairing here: the writer recorded it.
            let waiting = root.waiting().get(tx.db())?.unwrap_or_default();
            let rest: Vec<String> = waiting
                .into_iter()
                .filter(|p| !flushed.contains(p))
                .collect();
            tx.write(root.waiting().set(&rest));
            tx.write(root.last_arenas().set(arenas));
            tx.write(root.last_flushed().set(flushed));
        }
    }
    Ok(())
}

pub fn schema() -> Shape {
    <Ladder as Describe>::shape()
}

/// Write the old tape the way the old writer did (via the runtime).
pub fn write_old_tape(dir: &Path) -> Result<PathBuf> {
    let log = dir.join("ladder.jsonl");
    let rt = Runtime::open(dir.join("proj-old"), &log, schema(), None, reduce_old)?;
    for p in ["ann", "bo", "cy", "di", "ed"] {
        rt.append(Old::Joined { player: p.into() })?;
    }
    rt.append(Old::RoundBegan {
        round: 1,
        map: "dustbowl".into(),
    })?;
    rt.append(Old::Joined {
        player: "fay".into(),
    })?;
    rt.append(Old::RoundBegan {
        round: 2,
        map: "2fort".into(),
    })?;
    Ok(log)
}

/// The rewrite map: every RoundBegan becomes a RoundPaired whose arenas are
/// computed from the state BEFORE that record (`step.before`), by the old rule.
pub fn map_old_to_new(step: Step<'_, Old>) -> Result<Emit<New>> {
    Ok(match &step.rec.event {
        // Same bytes under both vocabularies: carried over untouched.
        Old::Joined { .. } => Emit::Keep,
        Old::RoundBegan { round, map } => {
            let waiting = Ladder::root()
                .waiting()
                .get(step.before)?
                .unwrap_or_default();
            let (arenas, flushed) = legacy_pairing(&waiting);
            Emit::Replace(New::RoundPaired {
                round: *round,
                map: map.clone(),
                flushed,
                arenas,
            })
        }
    })
}

/// Run the whole procedure in `dir`. Returns the new tape's path.
pub fn run(dir: &Path) -> Result<PathBuf> {
    let src = write_old_tape(dir)?;
    let dry = dry_run::<Old, New>(&src, schema(), reduce_old, Mode::SameSeq, map_old_to_new)?;
    print!("{}", dry.to_text());
    let dst = dir.join("ladder.round-paired.jsonl");
    rewrite::<Old, New>(
        &src,
        &dst,
        schema(),
        reduce_old,
        Mode::SameSeq,
        map_old_to_new,
    )?;
    check_rewrite_equivalent::<Old, New>(
        &src,
        &dst,
        schema(),
        reduce_old,
        reduce_new,
        app_keys_only,
    )
    .map_err(durable::Error::Reducer)?;
    println!("equivalent: old tape + old fold == new tape + new fold");
    Ok(dst)
}

fn main() {
    let dir = std::env::temp_dir().join(format!("durable-round-paired-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    match run(&dir) {
        Ok(dst) => println!("new tape: {}", dst.display()),
        Err(e) => {
            eprintln!("rewrite refused: {e}");
            std::process::exit(1);
        }
    }
}
