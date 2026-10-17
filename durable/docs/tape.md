# The tape: a self-sealing JSONL event log

*Status: specification, not yet implemented. Replaces the unframed
`JsonlLog` in `src/runtime.rs`.*

The tape is one append-only JSONL file. Every line is one event. It stays
greppable, `tail -f`-able and `jq`-able. What changes is that the bytes
themselves now say which events are committed, so a crash, a full disk or
an I/O error can never leave the tape in a state that needs a human to
judge it.

## Why

A production deploy (webmge, Fly.io, 1 GB volume) filled its disk to 98%.
Two tapes were torn mid-line by appends that hit ENOSPC:

```
identity catch_up failed: invalid identity log line 3463: EOF while parsing a string
ladder catch_up failed: invalid ladder log line 558: EOF while parsing a string
```

The old format has no way to tell a torn tail from corruption, so every
subsequent boot failed until someone truncated the files by hand. A
heuristic "drop the last line if it doesn't parse" repair was rejected:
it can't distinguish an interrupted append from damage to a committed
event.

sqlite also recovers at open (hot-journal rollback, discarding WAL frames
past the last valid commit). It never needs judgment because its format
defines what is committed. This spec does the same for one JSONL file.

## The invariant and the rule

**Invariant.** One writer per tape. It appends a whole batch, `fsync`s,
and only then may start the next batch. So at any moment at most one
batch — the *in-flight* batch — is not known to be durable, and it always
starts at seq `committed.len`.

**Rule.** The tape is the longest prefix of the file made of complete,
consistent, checksum-valid batches with contiguous seqs. Bytes after that
prefix belong to the in-flight batch and were never committed. A valid
line after that prefix that does *not* belong to the in-flight batch is
proof that acknowledged bytes were damaged: corruption.

## Fault model

| Event | Outcome |
|---|---|
| Crash, power loss, ENOSPC or I/O error during the in-flight batch | Detected. Recovery keeps the prefix of complete batches. |
| Damage to acknowledged bytes, with any line of a later batch surviving | Detected. Fatal, file untouched. |
| Tape shorter than what the projection already applied | Detected. Fatal, file untouched ("disagree", cause not asserted). |
| Damage to or loss of the final acknowledged batches beyond what the projection reached | **Not detectable** from inside the file. sqlite's WAL has the same limit. Backups/replication cover it. |

Assumed: `fsync` tells the truth; appending does not corrupt sectors
already on disk (sqlite's "powersafe overwrite" assumption); the
filesystem does not expose stale blocks in a file (ext4 `data=ordered`).

## Line format

```
{"seq":100,"ts_ms":1760684400123,"from":100,"to":102,"event":{…},"crc":"9f3a1c07"}
{"seq":101,"ts_ms":1760684400124,"from":100,"to":102,"event":{…},"crc":"0b77e3d1"}
{"seq":102,"ts_ms":1760684400125,"from":100,"to":102,"id":"op-7f2c","idh":"5e1d…","event":{…},"crc":"51e0b2aa"}
```

- `seq` — 0-based position on the tape. First, so `grep '^{"seq":10[0-9],'` works.
- `ts_ms` — monotonic millisecond timestamp stamped at ingest.
- `from`, `to` — first and last seq of this record's batch. A single
  append has `from == seq == to`.
- `id`, `idh` — optional, closing line only: the caller's idempotency
  key and a hash of the batch payload.
- `event` — the event body, nested so its fields can't collide with the envelope.
- `crc` — CRC32C, 8 lowercase hex digits, over the exact bytes of the
  line from `{` up to but not including `,"crc"`. The suffix always has the
  form `,"crc":"xxxxxxxx"}` (18 bytes), so it's computed over bytes as
  written and checked by slicing. No canonical JSON needed.
- Compact serde_json output; the only raw `\n` is the terminator.

Not used, on purpose: header lines, commit-marker lines, sidecar offset
files (each breaks "every line is an event"); chained CRCs (seq
contiguity already catches reordering and duplication, and unchained
lines stay checkable one at a time); whole-batch checksums (consistent
`(from, to)` plus contiguous seqs plus per-line CRCs already establish
completeness).

## The scan

One function, `scan(bytes) -> Ok(Committed{end, len, last_ts}) | Err(Corruption{offset, reason})`,
is used by the writer at open, by in-process readers, by `verify` and by
the CLI. Nothing else decides where the tape ends.

A *candidate line* is a physical line terminated by `\n`, no longer than
the maximum line length (default 16 MiB), whose CRC matches, whose
envelope decodes with `seq`, `from`, `to` as `u64`, and which satisfies
`from <= seq <= to` and `to - from < max_batch`. The scan never searches
for JSON fragments inside damaged bytes.

```
committed = {end: 0, len: 0, last_ts: 0}; state = Outside; bad = none
for each physical line L at offset P:
    if L is not a candidate line: bad = P; break
    match state:
      Outside:
        require L.from == committed.len && L.seq == L.from
        state = Inside{from: L.from, to: L.to, next: L.seq + 1}
      Inside s:
        require L.from == s.from && L.to == s.to && L.seq == s.next
        s.next += 1
    on require failure: bad = P; break
    if L.seq == L.to:
        committed = {end: P + len(L) + 1, len: L.to + 1, last_ts: max(...)}
        state = Outside
if bad:
    for each candidate line T in bytes[committed.end ..]:
        if T.from != committed.len: return Corruption{offset: bad}
return committed
```

Why the tail check is sound: every candidate line after `committed.end`
with `from == committed.len` is residue of the in-flight batch, including
the case where its closing line survived and an earlier line did not. A
candidate line with any other `from` means a later batch was written,
which means the batch before it was already fsynced and acknowledged, so
the bad bytes are damage to acknowledged data.

Blank lines are skipped (legacy compatibility).

## Opening

1. **Open and lock.** Open the tape. If it was just created, fsync the
   parent directory. Take `flock(LOCK_EX | LOCK_NB)` on the tape file
   (lock scope is one tape; tapes in the same directory never block each
   other). Then compare the inode of the path with the inode of the open
   handle; if they differ, the file was replaced while we waited — reopen
   and retry.
2. **Scan read-only.** No mutation yet. Corruption → stop, report, file
   untouched.
3. **Open the projection** and read `applied`. If RocksDB won't open,
   report the scan result and stop, file untouched.
4. **Cross-check.** If `applied > committed.len`: stop with "projection
   and tape disagree (applied=N, tape has M); possible committed-event
   loss or mismatched restore". File untouched. If the projection is
   missing or fresh, there is no evidence to check, and recovery proceeds
   under the fault model.
5. **Seal the boundary.** If `committed.end < file_len`, `ftruncate` to
   `committed.end`. Then `sync_data` **unconditionally**: a restarted
   process may be reading a complete batch that a crashed process left in
   the page cache without ever confirming it durable. If this sync fails,
   the open fails. Log one line: `tape: dropped N uncommitted bytes past seq S`.
6. **Publish** the committed offset to readers and **catch up** (see
   *Reducer failures*). Only now may idempotency lookups be answered.

Truncation at step 5 is not a repair. It removes only bytes the format
says are not on the tape, makes no judgment, needs no quarantine file
(nothing committed is removed), and is idempotent if open is interrupted
and rerun. It is required, not cosmetic: the writer uses `O_APPEND`, and
the next batch must not land behind dead bytes.

A poisoned writer (below) recovers only by going through this whole
sequence again.

## Appending

Under the ingest lock:

1. If the runtime is halted or poisoned, return that.
2. If `id` is given: look up `__durable/ids/<id>`. Same `idh` → `Duplicate(records)`;
   different `idh` → `Conflict`.
3. **Single event:** run the reducer into a `Tx` first, with today's
   `peek` semantics (committed state). Error → `Rejected`; nothing is
   written. This keeps poison events off the tape on the hot path
   (`store.append`).
4. Render all lines into one buffer in memory.
5. **Space.** Check the free-space floor via `statvfs` (configurable,
   e.g. 64 MiB or 2%); below it → `NotCommitted(NoSpace)`. Keep blocks
   reserved past EOF with `fallocate(FALLOC_FL_KEEP_SIZE)` in 16 MiB
   steps (file size and `tail` output unchanged); if that fails, retry with
   exactly the buffer size, then → `NotCommitted(NoSpace)`. Skip
   reservation where `fallocate` is unsupported.
6. `write_all(buffer)`, then `sync_data`.
7. Publish the new committed offset.
8. Single event: commit its `Tx` plus `applied` (`DisableWal`). Batch:
   reduce and commit event by event as today. Projection commit failure →
   `CommittedProjectionBehind`; the next append's catch-up retries.

### Outcomes

| Where it fails | Result | Writer | Why |
|---|---|---|---|
| Reducer (single event) | `Rejected` | healthy | Tape untouched |
| Render, floor, reserve | `NotCommitted` | healthy | Tape untouched |
| `write_all` partial | `NotCommitted` | healthy if `ftruncate(start)` + `sync_data` succeed, else poisoned | The closing line's `\n` can't have been written, so the batch is unclosed and the scan drops it regardless. Truncation only keeps `O_APPEND` from burying garbage. |
| `sync_data` | `NotCommitted` if `ftruncate(start)` + `sync_data` succeed, else `Unknown` | **poisoned** either way | Bytes may reach disk as a closed batch. After a failed fsync Linux may mark dirty pages clean; never retry and trust it. |
| Projection commit | `CommittedProjectionBehind` | healthy | Tape is truth |
| Process dies | (no reply; caller treats as unknown) | — | Next open decides |
| Success | `Committed(records)` | healthy | |

ENOSPC is normally caught before the tape is touched, and otherwise
rolled back. If the rollback itself can't be confirmed, the outcome is
`Unknown`, never a partially committed batch.

## Idempotency

A crash between fsync and reply leaves a durable batch whose caller never
heard. Only deduplication fixes that.

- `id` and `idh` (hash of the batch's event payloads) go on the closing line.
- The projection entry `__durable/ids/<id> = (from, idh)` is written in
  the same commit that applies the closing line, so it exists only once
  the whole batch has been applied. A crash mid-batch resumes from
  `applied` and writes it when catch-up reaches the closing line.
- Lookups run only after catch-up succeeds. A halted runtime returns
  `Halted` for every append and never appends, so a retry can't create a
  second copy; after the reducer is fixed and catch-up completes, the
  retry gets `Duplicate`.
- hop mints a fresh operation id per `store.append`. A flow id is only
  suitable if the flow appends exactly once.
- Expiry of old ids is a later projection policy.

## Reducer failures during replay

An event accepted at ingest that the current reducer rejects is a code
change, not a storage fault. Skipping it would build a projection
reflecting a history that never happened (a skipped credit followed by an
applied spend), and that projection can authorize irreversible actions.

Default: stop advancing the projection and halt with
`Halted{seq, err}`. Skipping is allowed only through an explicit,
domain-specific migration.

## Readers and backups

A complete batch is visible through the page cache before its fsync
returns. So:

- In-process readers stop at the **published committed offset**, never at
  the last syntactically complete batch.
- Backups go through the runtime: snapshot up to the committed offset, or
  copy the file and truncate the copy to the offset the live runtime
  reports. A raw live `cp` can capture a batch the primary later loses,
  after which the primary reuses that seq for a different event and the
  two histories diverge.
- `tail`, `grep` and `jq` on the raw file are for looking, not for backups.

## Old (unsealed) tapes

There is no legacy path in the writer or at open. Old tapes are migrated
offline, with no writer running, by `durable-tape-migrate [--drop-torn-tail] <tape>`:

- It writes `<tape>.migrated`, fsynced, with each old record as its own
  sealed batch (`from = to = seq`), seq and ts_ms preserved, event bytes
  verbatim. The operator renames it over the tape and fsyncs the
  directory. The existing projection stays valid: seqs don't change.
- Any damaged line, a seq gap, or an already-sealed envelope refuses and
  migrates nothing.
- A torn tail (final bytes with no newline) refuses too, unless
  `--drop-torn-tail` is passed. The old writer fsynced only after whole
  lines, so that fragment was never acknowledged; the flag drops exactly
  it and prints it. A damaged line that is newline-terminated is
  acknowledged history and is never excused. This is the path for the
  identity line-3463 and ladder line-558 tapes.

Open never treats an unsealed line as residue. A complete line that is
valid JSON but has no `,"crc":"xxxxxxxx"}` suffix can't have come from the
sealed writer (a torn write never ends in `}\n`, and zero-filled pages are
not valid JSON), so the scan refuses with `Corruption` and the file stays
byte-identical. This matters because the natural response to "projection
and tape disagree" is to delete the projection and rebuild; without this
rule that deleted the whole unmigrated tape.

A sealed line whose CRC no longer matches still has the suffix shape and
is ordinary damage, handled by the scan rule above.

Tapes from other writers (webmge's identity, ladder and inventory
journals) migrate only if their envelopes map onto `{seq, ts_ms, event}`
without reassigning seq or ts. Check per tape before migrating, and carry
each event's bytes verbatim (re-serializing through a JSON value sorts
keys and turns integers past 64 bits into floats).

## Operator commands

- `durable tape status <tape>` — committed end, record count, last seq and
  ts, uncommitted tail bytes, sealed or legacy.
- `durable tape verify <tape>` — full scan.
- `durable tape cat <tape> [--from SEQ]` — committed records only, for `jq`.

## Deferred

- Reducing a whole multi-event batch in one `Tx` with read-your-writes
  `peek`. This changes semantics for existing reducers (a reducer that
  writes then peeks currently sees the old value) and needs an overlay
  over prefix deletes, `Sum` merges and collection metadata. Separate
  project.
- Atomic visibility of a whole batch in the projection.
- Fast-path checkpoint so open scans only the tail.

## Tests

Crash and recovery:

1. Byte-granular: for every byte k of a batch (proptest over batch sizes
   and contents), truncate at k, reopen; the tape equals the prior
   committed prefix, the next append succeeds, `verify()` passes.
2. Lost pages: zero-fill or garble random spans of the in-flight batch,
   including garbage followed by valid lines of the same batch.
3. **Closing line survives, an earlier line of its batch doesn't**, for
   every position in batches of size 2–8: batch dropped, boot succeeds.
4. Valid line with a different `from` after damage: `Corruption`.
5. Inconsistent `to` within a batch: that batch not committed.
6. Overlong line and corrupt envelope in the tail: classified without
   substring scanning.
7. Recovery idempotence: interrupt open between scan and truncate, and
   between truncate and sync, repeatedly; every reopen gives the same tape.
8. Crashed child leaves a complete unsynced batch in the page cache: the
   reopening process syncs before reporting `Committed` or `Duplicate` for it.

Corruption and evidence:

9. Bit flip in a committed line with later batches (CRC mismatch, and
   valid-JSON value change): `Corruption{offset}`, file unchanged.
10. Final-batch rot with no later batch and no projection evidence:
    dropped (documented behavior, asserted on purpose).
11. Damaged final acknowledged batch with a projection that applied it:
    open stops with the disagreement error, file bytes unchanged.
12. Tape cut at a batch boundary below `applied`: fatal, file unchanged.

Writer failures:

13. Fault-injecting `Sink`: short write at byte k, ENOSPC at k, EIO on
    `sync_data`, failure of the rollback `ftruncate`. Assert every row of
    the outcome table, including poisoned states.
14. Real ENOSPC on a loop-mounted ext4 of a few MB: `NotCommitted(NoSpace)`
    with tape bytes unchanged; free space; appends resume. Repeat with the
    floor disabled to exercise rollback. Also exhaust inodes.
15. `kill -9` soak: a child appends in a loop and reports acknowledged
    seqs through a pipe; SIGKILL at random; every acknowledged seq present
    and contiguous on reopen.

Semantics:

16. Ingest-time reducer error: `Rejected`, tape untouched.
17. Replay-time reducer error: runtime halts with `Halted{seq}`; retries
    return `Halted` and the tape length doesn't change; after a reducer
    fix, catch-up completes and the retry gets `Duplicate`.
18. Lost reply: commit, crash before return, reopen, retry same id →
    `Duplicate`, one copy on tape. Same id, different payload → `Conflict`.
19. Second opener of a tape fails on the lock; `verify` and the CLI read
    alongside a live writer. Rename race during sealing: the second opener
    ends up locked on the new inode.
20. Unsealed tapes: refused with the file byte-identical, with or without
    a projection, including after the projection is deleted.
