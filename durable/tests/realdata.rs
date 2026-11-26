//! Real-data check: copies of webmge's prod-shaped tapes (never the originals)
//! run through census, verify, an identity Keep rewrite, and a digest check.
//!
//! Source selection: `DURABLE_REAL_TAPES` (a dir holding `ladder/ladder.jsonl`,
//! `identity/identity.jsonl`, `inventory/inventory.jsonl`), else the 2026-11-07
//! prod snapshot, else webmge's committed prod-shape fixture. Skips with a
//! message if none is present, so the suite stays green on a bare checkout.

mod common;

use std::path::{Path, PathBuf};

use common::*;
use durable::{census, digest_pairs, fold_dump, rewrite, Emit, Mode, Result, Runtime, Step};

fn source_root() -> Option<PathBuf> {
    let candidates = [
        std::env::var("DURABLE_REAL_TAPES").ok().map(PathBuf::from),
        Some(PathBuf::from(
            "/home/tommy/webmge-backups/prod-snapshot-2026-11-07",
        )),
        Some(PathBuf::from(
            "/home/tommy/programming/webmge/server/testdata/prod-shape",
        )),
    ];
    candidates
        .into_iter()
        .flatten()
        .find(|p| p.join("ladder/ladder.jsonl").exists())
}

fn keep(_s: Step<'_, serde_json::Value>) -> Result<Emit<serde_json::Value>> {
    Ok(Emit::Keep)
}

#[test]
fn prod_shaped_tapes_census_verify_and_identity_rewrite() {
    let Some(root) = source_root() else {
        eprintln!("realdata: no webmge tapes found; skipping");
        return;
    };
    let work = tmp_dir("realdata");
    let mut report_lines = Vec::new();
    for name in ["ladder", "identity", "inventory"] {
        let original = root.join(format!("{name}/{name}.jsonl"));
        if !original.exists() {
            eprintln!(
                "realdata: {} missing; skipping that tape",
                original.display()
            );
            continue;
        }
        // Work on a copy: the original is never opened for writing.
        let src = work.join(format!("{name}.jsonl"));
        std::fs::copy(&original, &src).unwrap();
        let before = read_bytes(&src);

        // census + verify
        let c = census::census(&src, "type").expect("census of a real tape");
        assert_eq!(
            c.tail_bytes, 0,
            "{name}: prod tapes are sealed, no torn tail"
        );
        assert!(census::verify(&src).unwrap().records > 0);
        report_lines.push(format!("== {name} ==\n{}", c.to_table()));

        // identity rewrite: byte-identical, and the same projection digest
        let dst = work.join(format!("{name}.rewritten.jsonl"));
        let report = rewrite::<Totals, serde_json::Value, serde_json::Value>(
            &src,
            &dst,
            reduce_value,
            Mode::SameSeq,
            keep,
        )
        .expect("identity rewrite");
        assert_eq!(report.read, c.records);
        assert_eq!(report.written, c.records);
        assert_eq!(
            read_bytes(&dst),
            before,
            "{name}: identity rewrite must be byte-identical"
        );
        assert_eq!(read_bytes(&src), before, "{name}: source untouched");
        let d_src = digest_pairs(
            &fold_dump::<Totals, serde_json::Value>(&src, reduce_value).unwrap(),
        );
        let d_dst = digest_pairs(
            &fold_dump::<Totals, serde_json::Value>(&dst, reduce_value).unwrap(),
        );
        assert_eq!(d_src, d_dst, "{name}: fold digests must match");

        // The live runtime on a copy agrees with the offline fold.
        let proj = work.join(format!("{name}-proj"));
        let rt = Runtime::open::<Totals>(&proj, &dst, reduce_value).unwrap();
        rt.verify().unwrap();
        let live = rt.dump().unwrap();
        let folded = fold_dump::<Totals, serde_json::Value>(&dst, reduce_value).unwrap();
        assert_eq!(
            live.into_iter()
                .filter(|(t, k, _)| durable::testing::app_keys_only(t, k))
                .collect::<Vec<_>>(),
            folded
                .into_iter()
                .filter(|(t, k, _)| durable::testing::app_keys_only(t, k))
                .collect::<Vec<_>>(),
        );
        println!(
            "realdata {name}: records={} digest={:02x?}",
            c.records,
            &d_src[..8]
        );
    }
    println!("{}", report_lines.join("\n"));
    let _ = Path::new("");
}
