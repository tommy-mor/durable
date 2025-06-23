use durable::{Db, DurableMap, DurableVec};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Open a database
    let db = Db::open("nested_example_db")?;
    
    // Create a map where each user has a list of posts
    let user_posts: DurableMap<String, DurableVec<String>> = DurableMap::new_nested(&db, "user_posts");
    
    // Add posts for Alice
    println!("Adding posts for Alice...");
    let mut alice_posts = user_posts.entry("alice".to_string())?.or_default()?;
    alice_posts.push("Hello, world!".to_string())?;
    alice_posts.push("Rust is awesome!".to_string())?;
    alice_posts.push("Loving persistent data structures!".to_string())?;
    
    // Add posts for Bob
    println!("Adding posts for Bob...");
    let mut bob_posts = user_posts.entry("bob".to_string())?.or_default()?;
    bob_posts.push("First post".to_string())?;
    bob_posts.push("Learning Rust".to_string())?;
    
    // Add a post for Charlie in a chained call
    println!("Adding post for Charlie...");
    user_posts.entry("charlie".to_string())?.or_default()?.push("One-liner post!".to_string())?;
    
    // Read back Alice's posts
    println!("\nAlice's posts:");
    let alice_posts_read = user_posts.entry("alice".to_string())?.or_default()?;
    for i in 0..alice_posts_read.len()? {
        if let Some(post) = alice_posts_read.get(i)? {
            println!("  {}: {}", i + 1, post);
        }
    }
    
    // Read back Bob's posts
    println!("\nBob's posts:");
    let bob_posts_read = user_posts.entry("bob".to_string())?.or_default()?;
    for i in 0..bob_posts_read.len()? {
        if let Some(post) = bob_posts_read.get(i)? {
            println!("  {}: {}", i + 1, post);
        }
    }
    
    // Read back Charlie's posts
    println!("\nCharlie's posts:");
    let charlie_posts_read = user_posts.entry("charlie".to_string())?.or_default()?;
    for i in 0..charlie_posts_read.len()? {
        if let Some(post) = charlie_posts_read.get(i)? {
            println!("  {}: {}", i + 1, post);
        }
    }
    
    println!("\nDemonstration of persistence...");
    println!("Data is now persisted to disk. You can stop and restart this program,");
    println!("and all the posts will still be there!");
    
    println!("\nTotal users with posts: 3");
    println!("Alice has {} posts", alice_posts_read.len()?);
    println!("Bob has {} posts", bob_posts_read.len()?);
    println!("Charlie has {} posts", charlie_posts_read.len()?);
    
    Ok(())
}