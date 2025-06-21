# Why Durable? The Missing Abstraction Layer for Persistent Storage

## The Problem: The Abstraction Gap

Every database forces developers to translate between how they **think** about data and how they **store** it. This translation layer is where bugs hide, performance suffers, and development slows down.

### Example: Building a Multiplayer Game Leaderboard

Let's say you need to store player rankings by game mode, with history by day. Here's the data model in your head:

```
Game Mode → Day → List of (Player, Score)
```

#### With Raw Key-Value Stores (sled, RocksDB)

```rust
// Storing a score requires manual key construction
let key = format!("leaderboard:{}:{}:player:{}", game_mode, day, player_id);
db.insert(key.as_bytes(), score.to_le_bytes())?;

// Getting today's leaderboard? Manual prefix scan and deserialization
let prefix = format!("leaderboard:{}:{}:", game_mode, day);
let mut scores = Vec::new();
for item in db.scan_prefix(prefix.as_bytes()) {
    let (key, value) = item?;
    // Parse player_id from key string... hope the format is right
    // Deserialize score... hope it's the right type
    scores.push((player_id, score));
}
scores.sort_by_key(|(_, s)| *s);

// Want to know how many players played today? Another scan!
// Want to atomically update multiple scores? Write a transaction wrapper!
// Want to clean up old days? Manual prefix iteration and deletion!
```

**Problems:**
- String manipulation for every operation
- No type safety (everything is bytes)
- Manual implementation of collection semantics
- No atomicity across related keys
- Performance overhead from string parsing

#### With SQL Databases (SQLite, PostgreSQL)

```sql
CREATE TABLE leaderboards (
    game_mode VARCHAR(50),
    day DATE,
    player_id UUID,
    score INTEGER,
    PRIMARY KEY (game_mode, day, player_id)
);

-- Getting a leaderboard requires SQL
SELECT player_id, score 
FROM leaderboards 
WHERE game_mode = ? AND day = ?
ORDER BY score DESC;
```

```rust
// In Rust, you need an ORM or manual query building
let scores: Vec<(Uuid, i32)> = sqlx::query_as(
    "SELECT player_id, score FROM leaderboards WHERE game_mode = $1 AND day = $2 ORDER BY score DESC"
)
.bind(&game_mode)
.bind(&day)
.fetch_all(&pool)
.await?;
```

**Problems:**
- Impedance mismatch (relational model vs nested structures)
- SQL complexity for simple operations
- ORMs add abstraction layers and performance overhead
- Async runtime required even for local storage
- Schema migrations for every structural change

#### With Document Stores (MongoDB)

```javascript
// Document structure
{
  game_mode: "ranked",
  day: "2024-01-15",
  scores: [
    { player_id: "abc", score: 1500 },
    { player_id: "def", score: 1400 }
  ]
}

// But now you have a different problem: updating a single score
// requires loading and saving the entire document!
```

**Problems:**
- Not embedded (requires separate process)
- Document size limits
- Inefficient for partial updates
- Complex setup for local-first apps

## The Solution: Native Data Structures

With Durable, you express your data model directly:

```rust
let leaderboard = DurableMap::<String, DurableMap<u32, DurableVec<(PlayerId, Score)>>>::new(&db, "leaderboard")?;

// Store a score - reads like natural Rust code
leaderboard
    .entry(game_mode)?
    .or_default()?
    .entry(day)?
    .or_default()?
    .push((player_id, score))?;

// Get today's leaderboard - it's just a Vec
let mut today_scores = leaderboard
    .get(&game_mode)?
    .and_then(|mode| mode.get(&day).ok())
    .unwrap_or_default();
today_scores.sort_by_key(|(_, s)| *s);

// All operations are atomic, typed, and efficient
```

## Why This Matters

### 1. **Zero Translation Overhead**

Your mental model **is** the storage model. No more:
- String concatenation for keys
- Manual serialization/deserialization  
- SQL query construction
- Document structure mapping

### 2. **Composition Without Complexity**

Nested data structures "just work":

```rust
// A real-world example: user notifications by app by priority
let notifications = DurableMap::<UserId, 
    DurableMap<AppId, 
        DurableMap<Priority, 
            DurableVec<Notification>>>>::new(&db, "notifs")?;

// Natural access patterns
notifications
    .get(&user_id)?
    .get(&app_id)?
    .get(&Priority::High)?
    .iter()
    .take(10)  // Latest 10 high-priority notifications
```

Try implementing this with SQL joins or KV prefixes!

### 3. **Type Safety Throughout**

```rust
// This won't compile - type safety at every level
let score: String = leaderboard.get(&"chess")?.get(&20240115)?.get(0)?;
//          ^^^^^^ expected Score, found String

// With raw KV stores, this is a runtime error after deserialization
```

### 4. **Atomicity By Design**

```rust
// Multiple operations in one atomic batch
let mut batch = db.batch();
batch.vec_push(&game.players, new_player)?;
batch.map_insert(&game.scores, player_id, 0)?;
batch.map_increment(&game.stats, "player_count", 1)?;
batch.commit()?;  // All or nothing
```

### 5. **Performance Without Compromise**

- **Locality**: Related data stored contiguously (prefix design)
- **Zero-copy possible**: Direct memory mapping for read-heavy workloads
- **Streaming iteration**: No need to load entire collections
- **Bulk operations**: Native batch support

## Comparison Matrix

| Feature | Durable | sled/RocksDB | SQLite | MongoDB |
|---------|---------|--------------|---------|----------|
| **Native collections** | ✅ Built-in | ❌ DIY | ❌ Tables only | ⚠️ Documents |
| **Type safety** | ✅ Full | ❌ Bytes | ⚠️ ORM-dependent | ⚠️ Schema validation |
| **Nested structures** | ✅ Natural | ❌ Manual prefixes | ❌ Joins/JSON | ✅ Embedded docs |
| **Atomic operations** | ✅ Automatic | ⚠️ Manual batching | ✅ Transactions | ⚠️ Document-level |
| **Local/embedded** | ✅ Yes | ✅ Yes | ✅ Yes | ❌ Separate process |
| **Schema evolution** | ✅ Per-collection | ❌ DIY | ⚠️ Migrations | ✅ Flexible |
| **Memory efficiency** | ✅ Scan & stream | ✅ Manual | ⚠️ Query-dependent | ❌ Doc loading |

## Real-World Use Cases Where Durable Shines

### Local-First Sync Engine

```rust
// Sync state with conflict tracking
let sync_state = DurableMap::<RecordId, DurableMap<DeviceId, Version>>::new(&db, "sync")?;

// Natural conflict detection
let versions = sync_state.get(&record_id)?;
if versions.values().unique().count() > 1 {
    // Conflict detected - handle naturally
}
```

### Time-Series Analytics Cache

```rust
// Metrics by source by minute
let metrics = DurableMap::<Source, DurableMap<UnixMinute, DurableVec<Metric>>>::new(&db, "metrics")?;

// Natural windowing
let last_hour: Vec<Metric> = metrics
    .get(&source)?
    .range(now - 3600..=now)?
    .flat_map(|(_, minute_metrics)| minute_metrics.iter())
    .collect();
```

### Feature Flag System with History

```rust
// Flags by environment with change history
let flags = DurableMap::<Env, DurableMap<FlagName, DurableVec<(Timestamp, Value)>>>::new(&db, "flags")?;

// Natural audit trail
let history = flags.get(&Env::Prod)?.get("new-feature")?.iter().collect();
```

## The Bottom Line

**Durable isn't a better database - it's the missing abstraction layer that lets you use persistent storage like in-memory collections.**

Stop translating. Start building.

---

*Next: Read the [RFC](001.md) for implementation details, or jump to the [Quick Start Guide](quickstart.md).* 