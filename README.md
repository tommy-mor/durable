# Durable

RocksDB-backed persistent data structures for Rust. Think `std::collections` but on disk!

## Features

- **Persistent Collections**: `DurableVec`, `DurableMap` (coming soon), `DurableSet` (coming soon)
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

## Current Status

### Implemented

- ✅ `DurableVec<T>` with full test coverage including:
  - Basic operations: `push`, `pop`, `get`, `len`, `clear`
  - Batch operations: `extend`
  - Iteration: `iter()` returns `Vec<T>` (streaming iteration coming soon)
  - Property-based testing with proptest
  - Unicode string support
  - Complex type support

### Coming Soon

- 🚧 `DurableMap<K, V>` - Persistent HashMap
- 🚧 `DurableSet<T>` - Persistent HashSet  
- 🚧 Streaming iterators for better memory efficiency
- 🚧 Collection nesting (e.g., `DurableMap<String, DurableVec<T>>`)
- 🚧 Schema migration support

## Performance

DurableVec operations are designed to be efficient:

- `push`: Single atomic write with WAL flush
- `get`: Direct key lookup, O(1) 
- `extend`: Batched writes for efficiency
- `clear`: Atomic batch deletion

## Testing

Run the test suite:

```bash
cargo test
```

Run the example:

```bash
cargo run --example vec_example
```

## License

Licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.

## Contributing

Contributions are welcome! Please feel free to submit a Pull Request.
