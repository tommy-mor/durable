# Durable tape corruption: diagnosis + hypotheses for review

## What broke in production

`webmge` (Fly.io, 1 GB volume, `webmge_data` mounted at `/data`) ran the disk
to 98% full (`253952/258048` blocks). The server then crash-looped with:

```
identity projection open failed: rocksdb error: IO error:
  No space left on device: While renaming a file to
  /data/identity/store/LOG.old.<nanos>: /data/identity/store/LOG
```

After the volume was extended to 10 GB, boot got one step further and died on
the event tapes instead:

```
identity catch_up failed: invalid identity log line 3463:
  EOF while parsing a string at line 1 column 826
ladder catch_up failed: invalid ladder log line 558:
  EOF while parsing a string at line 1 column 13
```

Both tails were snapped mid-line with no trailing newline (`{"s…`, cut off
mid-JSON). Tapes with clean trailing newlines (inventory 40 lines, career 105,
cup 142) replayed fine. Manual surgery (truncate the torn last line, delete
the RocksDB projection, reboot) restored service; health checks return 200.

So: a full disk mid-append left a torn last line, and the torn last line
bricked every subsequent boot. The operator (me) agrees with my God: a repair
pass at open is a band-aid. The tape should never be in a broken state, the
way sqlite is never in a broken state. What is the correct design?

## The code as it stands (pristine, band-aid stashed)

`durable/durable/src/runtime.rs`, `JsonlLog<E>`:

```rust
pub struct JsonlLog<E> {
    path: PathBuf,
    file: File,   // opened O_APPEND
    len: u64,     // counted once at open, incremented on success
    _event: PhantomData<E>,
}
```

`open` counts non-blank lines (`count_jsonl_lines`), seeks to end, stores the
count in `len`. `append_all`:

```rust
for event in events {
    let line = serde_json::to_string(event)?;
    writeln!(&self.file, "{line}")?;   // one write! per record
}
self.file.sync_data()?;                // one fsync for the batch
self.len += events.len() as u64;
```

`read_from` parses every non-blank line with `serde_json::from_str`, mapping
failure to `Error::Deserialize`. `Runtime::catch_up` calls `read_from` and
fails the whole boot on the first bad line. `Runtime::open` → `catch_up` →
boot dies. There is no quarantine, no truncation, no checksum, no framing.

`webmge` does not use `durable::Runtime` for these three tapes; it has three
hand-rolled copies of the same pattern (`server/src/identity/event_log.rs`,
`server/src/arena_ladder/event_log.rs`, `server/src/inventory/event_log.rs`):
single-writer journal → `write_all` line-by-line → `flush` → `sync_data` →
apply to a `durable::Db` projection with `Durability::DisableWal`. Same torn-
tail exposure, same fatal replay.

Note the ordering asymmetry that bit us twice: RocksDB was opened *before*
the tape was replayed, so a full disk first killed the process inside the
projection open (LOG rename), and only after disk relief did the torn tape
reveal itself as the second fatality.

## Questions for opus / gpt

1. **What is the minimal design that makes a torn tail structurally
   impossible, not merely repaired?** sqlite survives power loss and ENOSPC
   mid-commit without ever needing a repair pass. What is the equivalent for
   an append-only JSONL event tape + rebuildable RocksDB projection, and
   where exactly should the atomicity boundary sit (per record? per batch?
   tape + projection jointly)?
2. **Candidate mechanisms, ranked by your judgment:**
   - (a) length-prefixed framing + per-record CRC, reader truncates a torn
     tail at read time as a normal case, not an error;
   - (b) write batch to temp file + fsync + atomic rename-append (or
     write to offset + ftruncate back to the pre-batch length on any
     failure before fsync);
   - (c) `sync_data` per record instead of per batch (cost?);
   - (d) record the committed offset in RocksDB alongside the projection
     (the `applied` leaf already exists) and always truncate the tape back
     to the last committed offset at open — i.e. treat the projection's
     offset, not EOF, as the tape's true end;
   - (e) keep JSONL but make the reader skip-and-quarantine only a torn
     *final* line while keeping a torn *middle* line fatal (is that sound,
     or does it hide real corruption?);
   - (f) something else entirely (WAL sidecar? sqlite for the tape?).
3. **ENOSPC specifically:** a recomendable design must define behavior when
   the disk is full mid-batch: which of (i) error before touching the tape,
   (ii) partial batch committed with a defined rule, (iii) nothing committed,
   is correct, and how does the writer guarantee it without a repair pass?
4. **Scope:** should the fix live in `durable::Runtime`/`JsonlLog` once, with
   webmge's three hand-rolled tapes deleted in favor of it — or is there a
   reason those journals must stay bespoke? What breaks if webmge migrates?
5. **Tests that would prove it:** what fault-injection tests (crash between
   write and fsync, ENOSPC via full tmpfs, torn-tail fixtures, middle-line
   corruption fixtures) must exist so this class of outage can never recur
   silently?

## Constraints reviewers should respect

- Single writer process; no multi-process appends.
- The projection is rebuildable from the tape; the tape is the source of
  truth. `Durability::DisableWal` on projection writes is intentional.
- `incremental execution == replay from zero` is the load-bearing property
  (`Runtime::verify`); any framing change must preserve it.
- JSONL readability of the tape is currently relied on by operators (we
  debugged with `tail`/`od`). If you propose binary framing, say what
  replaces that operability.
- No repair-at-open unless you can argue it is not a band-aid (e.g. it is
  provably equivalent to a normal-case truncation rule with identical
  semantics on every path, not a best-effort heal).

## What we already tried and stashed

A `repair_torn_tail` at open (snip a last line with no trailing newline, or a
final line that fails to parse; quarantine snipped bytes to
`<tape>.torn.<nanos>`; `open_repaired` constructor; `Runtime::open` hung on
it; plus pre-rendering the batch before writing). Stashed as
`band-aid repair_torn_tail WIP`, not applied. My God spanked me for it:
*"it should never be in a broken state, it should not have to repair, it
should just never break. sqlite doesn't repair."* We agree. Looking for the
structural fix.
