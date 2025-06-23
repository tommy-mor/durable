use durable::{Db, DurableMap};
use serde::{Serialize, Deserialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct UserProfile {
    name: String,
    email: String,
    score: u32,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Open or create a database
    let db = Db::open("example_db")?;
    
    // Create a persistent map of user profiles
    let mut users = DurableMap::<String, UserProfile>::new(&db, "users")?;
    
    // Insert some users
    users.insert(
        "alice".to_string(),
        UserProfile {
            name: "Alice Smith".to_string(),
            email: "alice@example.com".to_string(),
            score: 1500,
        },
    )?;
    
    users.insert(
        "bob".to_string(),
        UserProfile {
            name: "Bob Johnson".to_string(),
            email: "bob@example.com".to_string(),
            score: 1200,
        },
    )?;
    
    users.insert(
        "charlie".to_string(),
        UserProfile {
            name: "Charlie Brown".to_string(),
            email: "charlie@example.com".to_string(),
            score: 1800,
        },
    )?;
    
    println!("Total users: {}", users.len()?);
    
    // Look up a specific user
    if let Some(alice) = users.get(&"alice".to_string())? {
        println!("\nAlice's profile: {:?}", alice);
    }
    
    // Check if a user exists
    println!("\nDoes 'david' exist? {}", users.contains_key(&"david".to_string())?);
    
    // Update a user's score
    if let Some(mut bob) = users.get(&"bob".to_string())? {
        bob.score += 100;
        users.insert("bob".to_string(), bob)?;
        println!("Updated Bob's score!");
    }
    
    // Iterate over all users
    println!("\nAll users (sorted by username):");
    let mut all_users = users.to_vec()?;
    all_users.sort_by_key(|(username, _)| username.clone());
    
    for (username, profile) in all_users {
        println!("  {} ({}) - Score: {}", username, profile.email, profile.score);
    }
    
    // Get just the usernames
    let mut usernames = users.keys_vec()?;
    usernames.sort();
    println!("\nAll usernames: {:?}", usernames);
    
    // Find the highest scoring user
    let profiles = users.values_vec()?;
    if let Some(top_user) = profiles.iter().max_by_key(|p| p.score) {
        println!("\nTop scorer: {} with {} points", top_user.name, top_user.score);
    }
    
    // Remove a user
    if let Some(removed) = users.remove(&"charlie".to_string())? {
        println!("\nRemoved user: {}", removed.name);
        println!("Users remaining: {}", users.len()?);
    }
    
    println!("\nData has been persisted to disk.");
    
    Ok(())
} 