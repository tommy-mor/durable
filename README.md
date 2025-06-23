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
    scores.insert("Alice".to_string(), 100)?;
    scores.insert("Bob".to_string(), 85)?;
    
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
  - Basic operations: `insert`, `get`, `remove`, `contains_key`, `len`, `clear`
  - Batch operations: `extend`
  - Iteration: `iter()`, `keys()`, `values()`
  - Complex key and value types
  - Property-based testing with proptest

### Coming Soon

- 🚧 `DurableSet<T>` - Persistent HashSet  
- 🚧 Collection nesting (e.g., `DurableMap<String, DurableVec<T>>`)
- 🚧 Schema migration support
- 🚧 Batch operations across multiple collections
- 🚧 Streaming iterators for `DurableMap`

## Performance

All operations are designed to be efficient:

- **DurableVec**:
  - `push`: Single atomic write with WAL flush
  - `get`: Direct key lookup, O(1) 
  - `extend`: Batched writes for efficiency
  - `clear`: Atomic batch deletion

- **DurableMap**:
  - `insert`/`get`: Direct key lookup, O(1) average
  - `remove`: Single delete with WAL flush
  - `len`: O(n) scan (can be optimized with metadata)
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
```

## License

Licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.

## Contributing

Contributions are welcome! Please feel free to submit a Pull Request.
