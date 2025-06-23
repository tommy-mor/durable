use durable::{Db, DurableMap, DurableVec};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let db = Db::open("streaming_demo_db")?;
    
    // Create collections with a moderate amount of data
    let mut map = DurableMap::<u32, String>::new(&db, "large_map")?;
    let mut vec = DurableVec::<String>::new(&db, "large_vec")?;
    
    println!("🚀 Streaming Iterator Demo\n");
    
    // Add 1000 entries to demonstrate streaming
    println!("Adding 1000 entries to map and vec...");
    for i in 0..1000 {
        map.insert(i, format!("Value {}", i))?;
        vec.push(format!("Item {}", i))?;
    }
    
    println!("\n📊 Collection sizes:");
    println!("  Map entries: {}", map.len()?);
    println!("  Vec elements: {}", vec.len()?);
    
    // Demonstrate streaming iteration - memory efficient
    println!("\n✨ Streaming iteration (memory efficient):");
    
    // Count items without loading into memory
    let map_count = map.iter().count();
    println!("  Counted {} map entries without loading into memory", map_count);
    
    // Find specific items efficiently
    let target = 500;
    let found = map.iter()
        .find(|item| {
            item.as_ref()
                .map(|(k, _)| *k == target)
                .unwrap_or(false)
        });
    
    if let Some(Ok((k, v))) = found {
        println!("  Found key {} with value '{}' via streaming", k, v);
    }
    
    // Process only what we need
    println!("\n🎯 Processing first 10 items only:");
    for (i, item) in vec.iter()?.take(10).enumerate() {
        match item {
            Ok(value) => println!("  [{}] {}", i, value),
            Err(e) => println!("  [{}] Error: {:?}", i, e),
        }
    }
    
    // Filter and process without loading all data
    println!("\n🔍 Filtering even keys without loading all data:");
    let even_count = map.keys()
        .filter(|item| {
            item.as_ref()
                .map(|k| k % 2 == 0)
                .unwrap_or(false)
        })
        .count();
    println!("  Found {} even keys", even_count);
    
    // Compare with loading everything into memory
    println!("\n⚠️  Loading all data into memory (less efficient for large collections):");
    let all_values = map.values_vec()?;
    println!("  Loaded {} values into a Vec", all_values.len());
    
    println!("\n✅ Streaming iterators provide:");
    println!("  • Constant memory usage regardless of collection size");
    println!("  • Ability to process data larger than RAM");
    println!("  • Early termination when finding specific items");
    println!("  • Efficient filtering and transformation");
    
    // Clean up
    drop(map);
    drop(vec);
    drop(db);
    std::fs::remove_dir_all("streaming_demo_db").ok();
    
    Ok(())
} 