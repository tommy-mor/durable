# One test suite, three engines: RocksDB, redb, and sled behind a cargo feature

`durable` no longer has a storage engine. It has a storage *choice*:

```toml
# the default, as before
durable = "0.3"
# or a pure-Rust backend — no C++ toolchain, no 10-minute RocksDB compile
durable = { version = "0.3", default-features = false, features = ["backend-redb"] }
durable = { version = "0.3", default-features = false, features = ["backend-sled"] }
```

Same API. Same bytes on disk per backend family. The full 115-test suite
passes **identically** on all three. This post is how we got there, what it
cost, what the numbers say, and the bug the benchmarks caught on day one.

Branch: `feat/store-backends`. Charts: [docs/bench/report-2026-10-19.html](https://github.com/tommy-mor/durable/blob/feat/store-backends/docs/bench/report-2026-10-19.html)
(raw JSON beside it if you want to argue with the methodology).

## Why: a compiler did this

This machine runs GCC 16. The vendored RocksDB 8.1.1 (via `rocksdb 0.21`)
doesn't compile on it — its headers use `uint32_t` without ever including
`<cstdint>`, which older libstdc++ happened to provide transitively. The
first fix was an env hack (`CXXFLAGS="-include cstdint"`). The second, real
realization: nothing in `durable` actually needs RocksDB *specifically*.
The entire storage surface turned out to be tiny:

- point `get`
- forward scans from a key (`IteratorMode::From`) and from the start
- atomic write batches: put / delete / merge / delete-range, with a
  sync / no-sync / no-WAL durability knob
- one associative merge operator folding tagged `Sum` accumulators

No column families. No transactions. No snapshots, checkpoints, or backups.
That fits in a trait — so we made it one. (Well, an enum. More on that.)

Two birds, one stone: along the way we bumped `rocksdb` 0.21 → 0.25
(RocksDB 8.1 → 11.8), which builds clean on GCC 16 with no flags at all.
The workaround is dead on every backend now.

## Design: an enum, not a trait object

`Db` talks to storage only through a `Store` enum (`Rocksdb | Redb | Sled`)
in the new `durable/src/store.rs`. Exactly one `backend-*` feature may be
enabled; anything else is a `compile_error!` with instructions.

The key structural move: `Batch` no longer streams into a
`rocksdb::WriteBatch`. It resolves to an ordered `FinalOp` stream —
put / delete / merge / delete-range — with prefix deletes and collection
appends resolved exactly as before (point ops first in arrival order, then
range expansion, then resolved appends). Every backend commits the same
flat op stream, atomically:

- **rocksdb**: the same `WriteBatch` as always. Zero behavior change.
- **redb**: one write transaction. Merges have no native equivalent, so
  they fold read-modify-write inside the commit txn through the shared
  pure-Rust `fold_merge` — exact under the single-writer contract durable
  already assumes.
- **sled**: `Batch` has no merge op either, so merges lower to put/delete
  against a commit-time overlay that replicates WriteBatch order
  semantics (later ops in the batch see earlier ones). Same shared fold.

Reads are owned-pair iterators on every backend. redb materializes scans
under one read transaction (genuinely snapshot-consistent, like RocksDB's
iterators); sled materializes eagerly (no read transactions there — still
exact for a single writer).

The `Sum` merge fold itself was extracted to a pure function both
backends share, so accumulator bytes are identical everywhere. Switching
backends for a live deployment is delete-the-store-dir + replay the tape —
which is the supported migration path anyway, since the store is always a
rebuildable projection.

## The bug the benchmarks caught

The new bench harness (`durable/examples/bench_backends.rs`) pushes to
*both ends* of one deque in a single batch — and the deque came back short:
1000 rows instead of 2000. Root cause: back-pushes and front-pushes
resolved as two groups with separate head/tail reads, so the second group
overwrote the first's counter update with a stale value. The existing test
suite never caught it because every test pushed one end per commit.

Fixed by resolving both ends against a single shared head/tail read
(preserving arrival order, which the old code didn't even do), with a
regression test. The harness earned its keep before measuring a single
nanosecond. This is the correct order of operations: benchmarks first,
then belief.

## Performance: disk doesn't break BigO

The harness (`--work` per backend, `--render` combines) sweeps N = 1k…1M
across List, Deque, Map, 3-deep nested maps, and record lists — each plotted
against its std counterpart (`memory`: Vec, VecDeque, BTreeMap) on log–log
axes with O(1)/O(N) slope guides. Cold reopen before every read phase,
medians over repeated samples.

Headliners from the full run:

- **Scans are O(N) everywhere**, on disk and in memory — same slopes on
  the log–log charts, separated by a constant that is the cost of the
  abstraction (point-gets + CBOR vs pointer chase).
- **Point gets are flat O(1)** at ~10µs cold on all three engines.
- **Bulk rebuild favors rocksdb ~10x** (6s vs ~60s per 2M rows) — LSM
  memtables earn their keep on write floods.
- **The map_clear chart is the money chart**: whole-subtree delete is flat
  34µs on rocksdb (range tombstone) and O(N) everywhere else — redb 32s,
  sled 45s, even BTreeMap 209ms at 1M rows. Know which one you're holding.
- **Single SyncWal commits**: rocksdb ~30µs, redb ~1.3ms, sled ~2.2ms —
  the fsync floor differs enormously. Batch your writes.

Caveats, stated plainly: datasets top out at 1M rows (below the >>RAM
ideal in `docs/bench-bigO.txt`), so constants are machine-specific;
slopes are the claim. Two infra notes: (1) one early sled run failed
with a file-lock conflict on a fresh tempdir and never reproduced in
three full runs since — still unexplained, watching; (2) a later run
died mid-leg with no trace at all (no error, no coredump, wrapper gone)
along with an unrelated long-lived server process — best explanation is
the background-shell supervisor reaping process groups, so long runs now
launch detached.

## Deliberately not a backend: SurrealKV

We recon'd `surrealkv` 1.0.0. Best semantic fit of all — native
`delete_range`, MVCC transactions, snapshots. Disqualified: its commit
path is `async` on tokio, this crate is fully synchronous, and the main
consumer commits from **tokio workers** (all three webmge journals), where
a `block_on` bridge would panic. That's not an inconvenience, it's a crash
landmine. Documented in `store.rs`; revisit if it ever grows a sync commit.

## Try it / what's next

```bash
git checkout feat/store-backends
cargo test                                          # rocksdb (default)
cargo test --no-default-features --features backend-redb
cargo test --no-default-features --features backend-sled
```

Open questions for the thread: should `backend-redb` become the default
for development (60s compiles beat 10-minute C++ builds)? Anyone want to
run the harness on spinning rust or a bigger box — the methodology section
in the report says exactly how. And the >>RAM runs from `docs/bench-bigO.txt`
(shrunk caches + eviction) are still on the todo list.
