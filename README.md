# Durable

RocksDB-backed persistent data structures for Rust. Think `std::collections` but on disk!

## Features

- **Persistent Collections**: `DurableVec`, `DurableMap`, `DurableSet` (coming soon)
- **Type-Safe**: Full Rust type safety with serde serialization
- **ACID Guarantees**: All operations are atomic and crash-safe
- **Zero-Copy Capable**: Efficient iteration without loading entire collections
- **Embedded**: No external services required - just a directory on disk

## Quick Start

Add to your `Cargo.toml`:

```toml
[dependencies]
durable = "0.1.0"
```

## Example

### DurableVec
```rust
use durable::{Db, DurableVec};
use serde::{Serialize, Deserialize};

#[derive(Debug, Serialize, Deserialize)]
struct Task {
    id: u64,
    title: String,
    completed: bool,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Open or create a database
    let db = Db::open("my_db")?;
    
    // Create a persistent vector
    let mut tasks = DurableVec::<Task>::new(&db, "tasks")?;
    
    // Use it like a normal Vec!
    tasks.push(Task {
        id: 1,
        title: "Build something amazing".to_string(),
        completed: false,
    })?;
    
    // Data persists across program restarts
    println!("Total tasks: {}", tasks.len()?);
    
    Ok(())
}
```

### DurableMap
```rust
use durable::{Db, DurableMap};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let db = Db::open("my_db")?;
    
    // Create a persistent map
    let mut scores = DurableMap::<String, u32>::new(&db, "scores")?;
    
    // Use it like a HashMap!
    // Use put() when you don't need the old value (more efficient)
    scores.put("Alice".to_string(), 100)?;
    scores.put("Bob".to_string(), 85)?;
    
    // Use insert() when you need to know the old value
    if let Some(old_score) = scores.insert("Alice".to_string(), 120)? {
        println!("Alice's previous score was: {}", old_score);
    }
    
    // Get values
    if let Some(score) = scores.get(&"Alice".to_string())? {
        println!("Alice's score: {}", score);
    }
    
    // Iterate over entries
    for (name, score) in scores.iter()? {
        println!("{}: {}", name, score);
    }
    
    Ok(())
}
```

### Nested Collections

Durable supports nesting collections within each other for complex data structures:

```rust
use durable::{Db, DurableMap, DurableVec};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let db = Db::open("my_db")?;
    
    // Create a map where each user has a list of posts
    let user_posts: DurableMap<String, DurableVec<String>> = 
        DurableMap::new_nested(&db, "user_posts");
    
    // Add posts for a user
    let mut alice_posts = user_posts.entry("alice".to_string())?.or_default()?;
    alice_posts.push("Hello, world!".to_string())?;
    alice_posts.push("Rust is awesome!".to_string())?;
    
    // Or use chained calls for convenience
    user_posts.entry("bob".to_string())?.or_default()?.push("First post!".to_string())?;
    
    // Access nested data
    let alice_posts = user_posts.entry("alice".to_string())?.or_default()?;
    println!("Alice has {} posts", alice_posts.len()?);
    
    Ok(())
}
```

The entry API automatically creates nested collections when they don't exist, providing ergonomic access patterns similar to `std::collections::HashMap::entry().or_default()`.

## Current Status

### Implemented

- ✅ `DurableVec<T>` with full test coverage including:
  - Basic operations: `push`, `pop`, `get`, `len`, `clear`
  - Batch operations: `extend`
  - Iteration: `iter()` returns a streaming iterator, `to_vec()` loads into memory
  - Property-based testing with proptest
  - Unicode string support
  - Complex type support

- ✅ `DurableMap<K, V>` with full test coverage including:
  - Basic operations: `insert`, `put`, `get`, `remove`, `contains_key`, `len`, `clear`
  - Batch operations: `extend`
  - Iteration: `iter()`, `keys()`, `values()` return streaming iterators
  - Memory loading: `to_vec()`, `keys_vec()`, `values_vec()` for convenience
  - Complex key and value types
  - Property-based testing with proptest

- ✅ **Nested Collections** with entry API:
  - `DurableMap<K, DurableVec<T>>` - Maps to vectors
  - `entry()` method with `or_default()` for ergonomic access
  - Automatic collection creation and management
  - Full persistence and isolation between nested collections
  - Type-safe compile-time enforcement

### Coming Soon

- 🚧 `DurableSet<T>` - Persistent HashSet  
- 🚧 Deep nesting (e.g., `DurableMap<String, DurableMap<String, DurableVec<T>>>`)
- 🚧 Schema migration support
- 🚧 Batch operations across multiple collections

## Performance

All operations are designed to be efficient:

- **DurableVec**:
  - `push`: Single atomic write with WAL flush
  - `get`: Direct key lookup, O(1) 
  - `len`: Metadata lookup, O(1)
  - `extend`: Batched writes for efficiency
  - `clear`: Atomic batch deletion

- **DurableMap**:
  - `insert`: Returns old value (2 ops: get + put), O(1) average
  - `put`: No return value (1 op: existence check + put), O(1) average
  - `get`: Direct key lookup, O(1) average
  - `remove`: Single delete with WAL flush
  - `len`: Metadata lookup, O(1)
  - `extend`: Batched writes for efficiency

## Testing

Run the test suite:

```bash
cargo test
```

Run the examples:

```bash
cargo run --example vec_example
cargo run --example map_example
cargo run --example combined_example  # Shows both collections working together
cargo run --example streaming_demo    # Demonstrates efficient streaming iteration
cargo run --example nested_example   # Shows nested collections (Map -> Vec)
cargo run --example simple_ranking   # Gaming leaderboard from docs/motivation.md
cargo run --example ranking_history  # Complex ranking system with persistence
```

## License

Licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.

## Contributing

Contributions are welcome! Please feel free to submit a Pull Request.
