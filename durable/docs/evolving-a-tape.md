# Evolving a tape

A durable application has two kinds of stored state: the **tape** (sealed JSONL,
the source of truth, never edited in place) and the **projection** (derived,
rebuilt from the tape). Changing how the application interprets history touches
one or both. This guide says which tool to use, and how to prove the change.

## Decide what kind of change it is

| The change | Tool | Tape changes? |
|---|---|---|
| A fold rule changed and history should be re-derived by the new rule (trophy rules, ELO policy, a new derived leaf) | Bump `ProjectionId::reducer` | No |
| A decision a later record depends on was made by a fold (pairing, a timeout winner) | Record the decision as data, then rewrite old records once | Yes, offline |
| A new envelope field or event kind is needed | Additive variant; strict envelope refuses it on older binaries | No |
| Old events are obsolete and must go | Offline rewrite that drops them (`Mode::Renumber`) | Yes, offline |

Never add a per-record version field to the envelope. The strict envelope
refuses unknown keys, so old binaries refuse new records loudly. Compat code
kept alive in reducers is what this machinery exists to delete.

## Bump the projection id when history should be re-derived

```rust
let rt = Runtime::open_with(
    db_dir,
    tape,
    OpenOptions::new::<Ladder>(ProjectionId::new(7), reduce),
)?;
```

On open, the stored identity `{reducer, schema_hash}` is compared with the wanted
one **before any reducer call**. A mismatch with the default `OnMismatch::Rebuild`
wipes the projection and replays from zero. `OnMismatch::Refuse` returns
`Error::ProjectionMismatch` and touches nothing.

- Bump `reducer` when a fold rule changes. Nothing in code can detect that, so
  a human does it.
- The schema hash changes by itself when the projection layout, key types, or
  format version changes. You do not need to bump for layout changes, but a bump
  costs nothing.
- A projection is derived. A rebuild is always safe, only slower. Rebuild is the
  default for that reason.

## Record decisions as data

If a fold decides something a later record depends on (who plays whom, who won a
timeout), the fold is the wrong place for the decision. The writer decides and
records it in the event. The fold only applies it. Then the rule can change
without rewriting the tape, and the history stays exact.

## Rewrite the tape once, offline

Use `durable::rewrite` when old records must change meaning. The rewrite folds the
**old** tape with the **old** reducer into an in-memory `redb` scratch projection. For each record,
`map` receives `Step { rec, before }`, where `before` is the state **before** that
record is reduced. Decisions that depend on old state are computed from exactly
the state the old rule saw.

```rust
let report = durable::rewrite::<Ladder, Old, New>(
    &src,            // never written
    &dst,            // must not exist; published atomically at the end
    reduce_old,      // the rule that built the history
    Mode::SameSeq,   // or Mode::Renumber to drop records
    |step| match &step.rec.event {
        Old::Joined { .. } => Ok(Emit::Keep),
        Old::RoundBegan { round, map } => {
            let waiting = Ladder::root().waiting().get(step.before)?.unwrap_or_default();
            let (arenas, flushed) = legacy_pairing(&waiting);
            Ok(Emit::Replace(New::RoundPaired { round: *round, map: map.clone(), flushed, arenas }))
        }
    },
)?;
```

Worked, runnable example: `cargo run -p durable --example rewrite_round_paired`
(source: `durable/examples/rewrite_round_paired.rs`, tested in
`durable/tests/worked_example.rs`, including a control where ignoring the
pre-record state is caught).

### What `rewrite` guarantees

- **Byte identity.** `Keep` copies the source line byte for byte (CRC included).
  `SameSeq` with all `Keep` is the identity on the file. `Renumber` copies a line
  whole when its seq, from and to do not change; otherwise it re-seals around the
  same event bytes.
- **Source is read-only.** It is opened read-only, shared-locked, and refused if
  a live writer holds the exclusive lock. A torn tail is refused: open the tape
  once with the writer stopped to seal it first.
- **Atomic publication.** Output goes to a temp file beside `dst`, is fsynced,
  re-scanned, and must end exactly at EOF with the expected record count. Only
  then is it published with `hard_link`, which fails if `dst` exists. Every error
  before publication removes the temp and leaves `dst` absent.
- **Refusals** (nothing written): `dst` exists; source corrupt, torn, or locked;
  `map` returns `Drop` in `SameSeq`; an event does not decode as the old enum;
  `map` or the old reducer errors.

`durable::dry_run` runs the whole pipeline without writing anything and returns
the same report. `RewriteReport::to_text()` renders it as stable `key: value`
lines.

### Prove it

```rust
durable::testing::assert_rewrite_equivalent::<Ladder, Old, New>(
    &src, &dst, reduce_old, reduce_new, durable::testing::app_keys_only,
);
```

It folds both tapes and compares every application key. On mismatch it panics
with the first differing key and both values decoded. `app_keys_only`
ignores the `__durable` bookkeeping table (applied offset and identity stamp),
which a rewrite that drops records legitimately changes.

For a digest instead of a diff: `durable::fold_dump::<Ladder, _>(&tape, reducer)` and
`durable::digest_pairs(&dump)` (SHA-256, length-prefixed, stable across platforms).

## Deploy and rollback checklist

Run this for any rewrite that changes a production tape. Each step has a stop
condition.

1. **Snapshot.** Copy the tape and the projection directory to a backup location.
   Do not rely on the live volume. Stop condition: the copy's `durable-tape verify`
   is clean (exit 0).
2. **Stop writers.** No process may hold the tape. The tape lock makes a second
   `open_with` fail, and `rewrite` refuses a locked source, so a running writer
   shows up as an error, not as a race. Stop condition: `rewrite` runs.
3. **Census.** `durable-tape census <tape> --tag type` shows what is on the tape.
   Confirm the kinds you expect to rewrite and that no unexpected kind exists.
4. **Dry run.** Run the rewrite as `dry_run` and read the report. The `replaced`
   and `dropped` counts must match what you expect.
5. **Rewrite to a new file.** Never in place. `dst` must not exist.
6. **Equivalence.** `assert_rewrite_equivalent`, or `fold_dump` on both tapes with
   the same `app_keys_only` filter, and compare digests. Stop condition: equal.
   If not equal, stop. The rewrite is wrong or the old rule is not the one that
   built the history. Do not deploy.
7. **Verify the output.** `durable-tape verify <new>` is clean (exit 0).
8. **Swap.** Rename the new tape over the old one with the writer still stopped,
   then `sync` the directory.
9. **Projection.** Either delete the projection directory, or deploy with the new
   `ProjectionId` so the first open rebuilds it. Stop condition: the first open
   after deploy logs no `ProjectionMismatch` and the application's invariants hold.
10. **Keep the backup** until the next successful deploy has run for a while.

**Rollback** before step 8: remove the new file, restart the old binary. Nothing
changed.

**Rollback** after step 8: restore the snapshot tape, delete the projection, and
run the previous binary. The previous binary must still read the old event
vocabulary. This is why the old variants are deleted only in a later release,
after the rewritten tape has been running.

## Caveats

- A binary built before the identity feature ignores the stamp and advances
  `applied` under its own rules. Nothing in this crate can prevent that. The
  rollback procedure above deletes the projection for that reason.
- A legacy projection (written by `Runtime::open`, no stamp, `applied > 0`) is
  rebuilt once under `open_with`. It is never adopted, because no shape was
  recorded for it.
- The tape lock is advisory (`flock`). It stops two cooperating writers, not a
  process that ignores it. `Runtime::open` (legacy) takes no lock.
- `rewrite` needs a filesystem with hard links (`dst` is published with
  `hard_link`). On one without, it fails cleanly and writes nothing.
- A projection on the memory backend is ephemeral: every open replays the tape.
