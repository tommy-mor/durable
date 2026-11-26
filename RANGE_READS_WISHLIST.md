# Range reads: wishlist

Written from building `react-sorter` (an axum + React port of sorter v1 on a
durable tape + redb projection). The design held up: the reducer is the schema
and a replay is the migration. What's missing is a way to read *part* of a map.
Today every collection read loads the whole collection.

## What's there today (as of f99e953)

- `Path<Map<K, V>>::keys / iter / entries / len` all walk the whole child prefix
  and decode every key (and, for `iter`, every value).
  - `len` is `keys().len()`.
- The query language has `First`, `Last` and `Slice`, but they're applied after
  `collection_foci` has collected every child (`query.rs:512-531`). So
  `Slice { 0..50 }` over 18k votes still reads all 18k.
- **redb backend:** `redb_be::scan` copies the whole `[from, end)` range into a
  `Vec` before returning (`store.rs:270-291`). `Map::contains` and
  `Map::is_empty` call `iter_from(..).next()`, so on redb they copy the entire
  subtree just to look at its first key.
- **Sibling order isn't value order.**
  - A child segment is `uvarint(len) ++ [TAG_KEY] ++ cbor(key)`, so siblings
    sort by encoded length first, then by bytes.
  - That happens to match numeric order for non-negative integers.
  - Strings sort shortest-first: `"zz" < "aaa"`.
  - Negative integers are CBOR major type 1, so `-1` sorts after `0..23` and
    before `24`.
  - Tuple keys are CBOR arrays, so "every key whose first element is X" isn't a
    contiguous range.

## Use cases (all real, from react-sorter)

Numbers are from the sorter v1 import: 27.6k events, 18.3k votes, 3.9k items,
238 users.

1. **Newest-first pagination of an index.**
   - The `/users/$name` page shows 50 votes per page from
     `user_votes: Map<String, Map<i64 /*vote id*/, Leaf<bool>>>`.
   - The heaviest user has 2,347 votes. Every page view loads all 2,347 ids,
     sorts them, and keeps 50.
   - Wanted: the last 50 keys under a prefix, then the 50 before key K.
2. **Cursor pagination that's stable under appends.**
   - Offset paging (`skip(page * 50)`) shifts when a vote lands mid-browse.
   - "Keys after K" doesn't shift.
3. **Recent activity per item or tag.**
   - The busiest items have ~580 votes each in `item_votes`. The item page
     wants the 20 most recent.
   - A home-page "recent votes" feed wants the last N across `votes: Map<i64, Leaf<Vote>>`.
4. **Counts without decoding.**
   - The home page shows `users / items / votes` counts. Each is
     `Map::len`: three full key scans with CBOR decoding, on every uncached request.
   - Wanted: a key-only count that doesn't decode, or better, a count kept in
     the map's `META` that writes update.
5. **Existence checks.**
   - `contains` / `is_empty` should be one seek. On redb they currently copy
     the subtree (see above).
6. **Prefix search on string keys.**
   - The "whose votes" picker autocompletes over every user name. It
     currently ships all 238 names to the browser.
   - Wanted: keys in `users` starting with `"tom"`, limit 10.
   - Needs lexicographic string order. It's impossible with a length-first
     segment encoding.
7. **Range over the first element of a tuple key.**
   - `identities: Map<(provider, subject), Leaf<user>>`: wanted "every github
     identity".
   - Also wanted: a `(tag, attribute)` composite without nesting maps.
   - Today the workaround is nesting `Map<A, Map<B, V>>`. That works, but it
     forces the schema shape around the access path.
8. **Time-ordered secondary indexes.**
   - A `Map<(i64 /*ts_ms*/, i64 /*id*/), Leaf<()>>` index gives "everything in
     the last hour" or "votes between t0 and t1".
   - Works for non-negative ints today, if the range API existed.

## Proposed API

```rust
// Lazy, bounded, both directions. Nothing is read until iterated.
impl<K, V> Path<Map<K, V>> {
    fn range<'a>(&self, db: &'a Db, r: impl RangeBounds<K>) -> Result<MapRange<'a, K, V>>;
    fn first(&self, db: &Db) -> Result<Option<K>>;   // one seek
    fn last(&self, db: &Db) -> Result<Option<K>>;    // one reverse seek
    fn count(&self, db: &Db) -> Result<u64>;         // see use case 4
}
// MapRange: Iterator<Item = Result<(K, Path<V>)>> + DoubleEndedIterator
// and for Leaf maps, a value-decoding variant.

// Cursor page helper; `next` is the cursor for the following page.
fn page(&self, db: &Db, after: Option<&K>, limit: usize, dir: Dir)
    -> Result<(Vec<(K, T)>, Option<K>)>;

// String keys only, once ordering is lexicographic.
fn prefix(&self, db: &Db, p: &str) -> Result<MapRange<..>>;
```

For queries:
- Push `First`, `Last` and `Slice` down into a bounded store scan instead of
  slicing the collected foci.
- Add `Nav::Range { from, to, rev }` so remote readers get it too.
- The cost classes already model this: `First`/`Last` are `Bounded`, and a
  pushed-down `Slice` could be too.

## Backend work

- **Streaming redb scans.** Hold the `ReadTransaction` (and its opened table)
  in the iterator instead of collecting into a `Vec`. If the lifetimes fight
  back, a chunked iterator (seek, read N, re-seek after the last key) gets most
  of the benefit. It loses single-snapshot consistency across chunks, so
  document that, or keep the txn.
- **Reverse iteration.** redb's `Range` is double-ended. RocksDB has
  `Direction::Reverse`.
- `iter_from` already bounds scans by `prefix_upper_bound`, so the range API is
  mostly "pass a tighter `[start, end)` and stop materializing".

## Key encoding (the bigger ask)

For use cases 6 and 7, siblings must sort by key value, not by length:
- Replace the length prefix on child segments with an escaped, terminated
  encoding, like FoundationDB's tuple layer: `0x00` terminates and an
  embedded `0x00` becomes `0x00 0xFF`.
- Encode integer keys with the existing `order_i64` / `order_u64` instead of
  raw CBOR.
- Then:
  - strings sort lexicographically;
  - a tuple's first element is a contiguous prefix;
  - negative i64 keys sort correctly.

This is an on-disk format change. Projections are rebuildable by design, so
it costs a replay. Two things to watch:
- Watch prefixes (`watch.rs`) are built from the same codec.
- The public `codec` module (f99e953) has external readers that would need the
  new decoder.
- If that's too much, ship the range API on today's encoding first (use cases
  1-5 and 8 only need non-negative ints) and document the ordering caveats.

## Smaller related wishes

- **Rebuild on schema change.**
  - `Runtime::open_described` catches up from the cursor but can't tell that
    the `Shape` or the reducer changed.
  - react-sorter works around it with a `projection.version` file holding
    `REDUCER_VERSION:hash(format!("{:?}", Store::shape()))`, and calls
    `rebuild()` when it changes.
  - durable could store the shape fingerprint in `META` and rebuild
    automatically, given a caller-supplied reducer version.
- **Docs: key ordering.** The README should say how siblings sort. Right now
  you have to read `codec.rs` to find out.
