//! Projection equivalence: sorted multi-table dumps, SHA-256 digests, full-tape
//! in-memory folds, and testing helpers proving `old tape + old fold == new tape + new fold`.

use std::path::Path;

use serde::de::DeserializeOwned;
use sha2::{Digest as _, Sha256};

use crate::projection::{write_stamp, ProjectionStamp};
use crate::rewrite::for_each_committed;
use crate::runtime::{fold_one, Record};
use crate::store::{Db, Tx};
use crate::{Durability, Result, Schema};

/// A sorted dump of a database: `(table_name, key_bytes, value_bytes)`.
pub type Dump = Vec<(String, Vec<u8>, Vec<u8>)>;

/// Stable 32-byte SHA-256 digest of a sorted [`Dump`].
pub fn digest_pairs(pairs: &[(String, Vec<u8>, Vec<u8>)]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(b"durable.digest.v2\0");
    for (table, k, v) in pairs {
        h.update((table.len() as u64).to_be_bytes());
        h.update(table.as_bytes());
        h.update((k.len() as u64).to_be_bytes());
        h.update(k);
        h.update((v.len() as u64).to_be_bytes());
        h.update(v);
    }
    h.finalize().into()
}

/// Fold a whole tape (committed prefix only) with `reducer` into an in-memory
/// `redb` projection and return its sorted dump.
pub fn fold_dump<S: Schema, E: DeserializeOwned + 'static>(
    tape: &Path,
    reducer: impl Fn(&mut Tx, &E) -> Result<()>,
) -> Result<Dump> {
    let db = Db::open_memory()?;
    {
        let mut tx = db.write_with(Durability::DisableWal)?;
        write_stamp(&mut tx, ProjectionStamp::for_schema::<S>(0))?;
        tx.commit()?;
    }
    for_each_committed::<E>(tape, |rec: Record<E>| {
        fold_one(&db, &reducer, rec.meta(), &rec.event)
    })?;
    db.dump()
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

    use super::fold_dump;
    use crate::store::{Tx, BOOKKEEPING_TABLE};
    use crate::{Result, Schema};

    /// `true` for application entries: everything outside the `__durable`
    /// bookkeeping table.
    pub fn app_keys_only(table: &str, _key: &[u8]) -> bool {
        table != BOOKKEEPING_TABLE
    }

    /// Fold `old_tape` with `old_reducer` and `new_tape` with `new_reducer`,
    /// and compare the application keys accepted by `keep`.
    pub fn check_rewrite_equivalent<S, O, N>(
        old_tape: &Path,
        new_tape: &Path,
        old_reducer: impl Fn(&mut Tx, &O) -> Result<()>,
        new_reducer: impl Fn(&mut Tx, &N) -> Result<()>,
        keep: impl Fn(&str, &[u8]) -> bool,
    ) -> std::result::Result<(), String>
    where
        S: Schema,
        O: DeserializeOwned + 'static,
        N: DeserializeOwned + 'static,
    {
        let left: Vec<_> = fold_dump::<S, O>(old_tape, old_reducer)
            .map_err(|e| format!("fold of old tape failed: {e}"))?
            .into_iter()
            .filter(|(t, k, _)| keep(t, k))
            .collect();
        let right: Vec<_> = fold_dump::<S, N>(new_tape, new_reducer)
            .map_err(|e| format!("fold of new tape failed: {e}"))?
            .into_iter()
            .filter(|(t, k, _)| keep(t, k))
            .collect();
        if left == right {
            return Ok(());
        }
        let (mut i, mut j) = (0, 0);
        loop {
            match (left.get(i), right.get(j)) {
                (Some((lt, lk, lv)), Some((rt, rk, rv))) if (lt, lk) == (rt, rk) => {
                    if lv != rv {
                        return Err(format!(
                            "first differing key {lt}/{} \n  old: {}\n  new: {}",
                            show_key(lk),
                            show_value(lv),
                            show_value(rv)
                        ));
                    }
                    i += 1;
                    j += 1;
                }
                (Some((lt, lk, lv)), Some((rt, rk, _))) if (lt, lk) < (rt, rk) => {
                    return Err(format!(
                        "key {lt}/{} only in old projection\n  old: {}\n  new: <absent>",
                        show_key(lk),
                        show_value(lv)
                    ));
                }
                (Some(_), Some((rt, rk, rv))) => {
                    return Err(format!(
                        "key {rt}/{} only in new projection\n  old: <absent>\n  new: {}",
                        show_key(rk),
                        show_value(rv)
                    ));
                }
                (Some((lt, lk, lv)), None) => {
                    return Err(format!(
                        "key {lt}/{} only in old projection\n  old: {}\n  new: <absent>",
                        show_key(lk),
                        show_value(lv)
                    ));
                }
                (None, Some((rt, rk, rv))) => {
                    return Err(format!(
                        "key {rt}/{} only in new projection\n  old: <absent>\n  new: {}",
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
    pub fn assert_rewrite_equivalent<S, O, N>(
        old_tape: &Path,
        new_tape: &Path,
        old_reducer: impl Fn(&mut Tx, &O) -> Result<()>,
        new_reducer: impl Fn(&mut Tx, &N) -> Result<()>,
        keep: impl Fn(&str, &[u8]) -> bool,
    ) where
        S: Schema,
        O: DeserializeOwned + 'static,
        N: DeserializeOwned + 'static,
    {
        if let Err(diff) =
            check_rewrite_equivalent::<S, O, N>(old_tape, new_tape, old_reducer, new_reducer, keep)
        {
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
            Ok(v) => format!("{v:?}"),
            Err(_) => format!("<{} bytes: {}>", bytes.len(), hex(bytes)),
        }
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }
}
