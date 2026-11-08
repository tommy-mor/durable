//! Projection equivalence: dumps, digests, folds of a whole tape, and the
//! `testing` helpers that prove `old tape + old fold == new tape + new fold`.
//!
//! Dumps include the `__durable` bookkeeping keys (applied offset and identity
//! stamp). Equivalence between two different tapes compares application
//! state only: the `testing` helpers filter those keys out by default, because
//! a rewrite that drops or renumbers records legitimately changes `applied`.

use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use sha2::{Digest as _, Sha256};

use crate::projection::{stamp_writes_for, ProjectionStamp};
use crate::rewrite::for_each_committed;
use crate::runtime::{fold_one, Record, Tx};
use crate::{Db, Error, Result, Shape};

/// A full sorted dump of a database: every key and value, byte for byte.
pub(crate) fn dump_db(db: &Db) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let iter = db.iter_start()?;
    let mut out = Vec::new();
    for item in iter {
        out.push(item?);
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

/// Stable 32-byte digest of a sorted dump. Framing is length-prefixed
/// (big-endian `u64` lengths), so `("ab","c")` and `("a","bc")` differ, and
/// the domain tag keeps it distinct from any other SHA-256 in the crate.
pub fn digest_pairs(pairs: &[(Vec<u8>, Vec<u8>)]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(b"durable.digest.v1\0");
    for (k, v) in pairs {
        h.update((k.len() as u64).to_be_bytes());
        h.update(k);
        h.update((v.len() as u64).to_be_bytes());
        h.update(v);
    }
    h.finalize().into()
}

/// Fold a whole tape (committed prefix only) with `reducer` into a scratch
/// projection and return its sorted dump.
///
/// The scratch projection is stamped with `ProjectionStamp::new(0, schema)`,
/// so two folds of the same tape with the same schema produce identical dumps
/// and a `Runtime` opened with reducer id 0 over the same schema matches too.
pub fn fold_dump<E: DeserializeOwned + 'static>(
    tape: &Path,
    schema: Shape,
    reducer: impl Fn(&mut Tx, &E) -> Result<()>,
) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let scratch = scratch_dir(&std::env::temp_dir(), "fold")?;
    let result = (|| {
        let db = Db::open(&scratch)?;
        db.apply(
            &stamp_writes_for(ProjectionStamp::new(0, &schema)),
            crate::Durability::DisableWal,
        )?;
        for_each_committed::<E>(tape, |rec: Record<E>| {
            fold_one(&db, &reducer, rec.meta(), &rec.event)
        })?;
        dump_db(&db)
    })();
    let _ = std::fs::remove_dir_all(&scratch);
    result
}

/// A unique scratch directory under the system temp dir. Backends that are
/// ephemeral (`backend-memory`) ignore it; on-disk backends write into it.
pub(crate) fn scratch_dir(parent: &Path, label: &str) -> Result<PathBuf> {
    let dir = parent.join(format!("durable-{label}-{}", unique_suffix()));
    // `create_dir`, not `create_dir_all`: a directory that already exists is a
    // collision, and two folds sharing one database would be silent corruption.
    std::fs::create_dir(&dir).map_err(|e| Error::Log(format!("scratch {}: {e}", dir.display())))?;
    Ok(dir)
}

/// A name suffix unique within this process (atomic counter) and across
/// processes (pid, time).
pub(crate) fn unique_suffix() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{}-{nanos}-{n}", std::process::id())
}

/// Helpers for proving a rewrite preserved application state.
pub mod testing {
    use std::path::Path;

    use serde::de::DeserializeOwned;

    use super::{fold_dump, Result};
    use crate::{codec, runtime::META_NS, Shape, Tx};

    /// `true` for keys that are application data: everything outside the
    /// `__durable` bookkeeping namespace (applied offset, identity stamp).
    pub fn app_keys_only(key: &[u8]) -> bool {
        let mut meta = Vec::new();
        codec::put_segment(&mut meta, META_NS.as_bytes());
        !key.starts_with(&meta)
    }

    /// Fold `old_tape` with `old_reducer` and `new_tape` with `new_reducer`,
    /// and compare the application keys accepted by `keep`. `Err` carries a
    /// readable first difference: the key (hex and UTF-8 when valid), and both
    /// values decoded from CBOR to JSON when they decode.
    pub fn check_rewrite_equivalent<O, N>(
        old_tape: &Path,
        new_tape: &Path,
        schema: Shape,
        old_reducer: impl Fn(&mut Tx, &O) -> Result<()>,
        new_reducer: impl Fn(&mut Tx, &N) -> Result<()>,
        keep: impl Fn(&[u8]) -> bool,
    ) -> std::result::Result<(), String>
    where
        O: DeserializeOwned + 'static,
        N: DeserializeOwned + 'static,
    {
        let left: Vec<_> = fold_dump::<O>(old_tape, schema.clone(), old_reducer)
            .map_err(|e| format!("fold of old tape failed: {e}"))?
            .into_iter()
            .filter(|(k, _)| keep(k))
            .collect();
        let right: Vec<_> = fold_dump::<N>(new_tape, schema, new_reducer)
            .map_err(|e| format!("fold of new tape failed: {e}"))?
            .into_iter()
            .filter(|(k, _)| keep(k))
            .collect();
        if left == right {
            return Ok(());
        }
        let (mut i, mut j) = (0, 0);
        loop {
            match (left.get(i), right.get(j)) {
                (Some((lk, lv)), Some((rk, rv))) if lk == rk => {
                    if lv != rv {
                        return Err(format!(
                            "first differing key {}\n  old: {}\n  new: {}",
                            show_key(lk),
                            show_value(lv),
                            show_value(rv)
                        ));
                    }
                    i += 1;
                    j += 1;
                }
                (Some((lk, lv)), Some((rk, _))) if lk < rk => {
                    return Err(format!(
                        "key {} only in old projection\n  old: {}\n  new: <absent>",
                        show_key(lk),
                        show_value(lv)
                    ));
                }
                (Some(_), Some((rk, rv))) => {
                    return Err(format!(
                        "key {} only in new projection\n  old: <absent>\n  new: {}",
                        show_key(rk),
                        show_value(rv)
                    ));
                }
                (Some((lk, lv)), None) => {
                    return Err(format!(
                        "key {} only in old projection\n  old: {}\n  new: <absent>",
                        show_key(lk),
                        show_value(lv)
                    ));
                }
                (None, Some((rk, rv))) => {
                    return Err(format!(
                        "key {} only in new projection\n  old: <absent>\n  new: {}",
                        show_key(rk),
                        show_value(rv)
                    ));
                }
                (None, None) => {
                    return Err(format!(
                        "projections differ ({} vs {} keys) but no key differs",
                        left.len(),
                        right.len()
                    ))
                }
            }
        }
    }

    /// Panicking form of [`check_rewrite_equivalent`] for tests.
    pub fn assert_rewrite_equivalent<O, N>(
        old_tape: &Path,
        new_tape: &Path,
        schema: Shape,
        old_reducer: impl Fn(&mut Tx, &O) -> Result<()>,
        new_reducer: impl Fn(&mut Tx, &N) -> Result<()>,
        keep: impl Fn(&[u8]) -> bool,
    ) where
        O: DeserializeOwned + 'static,
        N: DeserializeOwned + 'static,
    {
        if let Err(diff) = check_rewrite_equivalent::<O, N>(
            old_tape,
            new_tape,
            schema,
            old_reducer,
            new_reducer,
            keep,
        ) {
            panic!("rewrite is not equivalent: {diff}");
        }
    }

    fn show_key(key: &[u8]) -> String {
        match std::str::from_utf8(key) {
            Ok(s) => format!("{s:?} (hex {})", hex(key)),
            Err(_) => format!("hex {}", hex(key)),
        }
    }

    fn show_value(bytes: &[u8]) -> String {
        match ciborium::de::from_reader::<ciborium::Value, _>(bytes) {
            Ok(v) => crate::dynpath::cbor_to_json(&v).to_string(),
            Err(_) => format!("<undecodable {} bytes: {}>", bytes.len(), hex(bytes)),
        }
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }
}
