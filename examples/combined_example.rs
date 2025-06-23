use durable::{Db, DurableMap, DurableVec};
use serde::{Serialize, Deserialize};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Message {
    id: u64,
    from: String,
    to: String,
    content: String,
    timestamp: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct User {
    username: String,
    display_name: String,
    message_count: u32,
}

fn get_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Open or create a database
    let db = Db::open("chat_db")?;
    
    // Create our collections
    let mut users = DurableMap::<String, User>::new(&db, "users")?;
    let mut messages = DurableVec::<Message>::new(&db, "messages")?;
    let mut user_message_indices = DurableMap::<String, Vec<usize>>::new(&db, "user_messages")?;
    
    // Create some users
    users.insert("alice".to_string(), User {
        username: "alice".to_string(),
        display_name: "Alice Smith".to_string(),
        message_count: 0,
    })?;
    
    users.insert("bob".to_string(), User {
        username: "bob".to_string(),
        display_name: "Bob Johnson".to_string(),
        message_count: 0,
    })?;
    
    users.insert("charlie".to_string(), User {
        username: "charlie".to_string(),
        display_name: "Charlie Brown".to_string(),
        message_count: 0,
    })?;
    
    // Helper to send a message
    let send_message = |from: &str, to: &str, content: &str, 
                        messages: &mut DurableVec<Message>,
                        users: &mut DurableMap<String, User>,
                        indices: &mut DurableMap<String, Vec<usize>>| -> Result<(), Box<dyn std::error::Error>> {
        // Create message
        let msg_id = messages.len()? as u64;
        let message = Message {
            id: msg_id,
            from: from.to_string(),
            to: to.to_string(),
            content: content.to_string(),
            timestamp: get_timestamp(),
        };
        
        // Store message
        messages.push(message)?;
        let msg_index = messages.len()? - 1;
        
        // Update sender's message count
        if let Some(mut sender) = users.get(&from.to_string())? {
            sender.message_count += 1;
            users.insert(from.to_string(), sender)?;
        }
        
        // Track message indices for recipient
        let mut recipient_indices = indices.get(&to.to_string())?.unwrap_or_default();
        recipient_indices.push(msg_index);
        indices.insert(to.to_string(), recipient_indices)?;
        
        Ok(())
    };
    
    // Send some messages
    println!("💬 Chat Application Demo\n");
    println!("Sending messages...");
    
    send_message("alice", "bob", "Hey Bob, how's the Durable library coming along?", 
                 &mut messages, &mut users, &mut user_message_indices)?;
    
    send_message("bob", "alice", "It's going great! We have DurableVec and DurableMap working!", 
                 &mut messages, &mut users, &mut user_message_indices)?;
    
    send_message("charlie", "alice", "That sounds awesome! Can I help with testing?", 
                 &mut messages, &mut users, &mut user_message_indices)?;
    
    send_message("alice", "charlie", "Absolutely! The more testing the better!", 
                 &mut messages, &mut users, &mut user_message_indices)?;
    
    send_message("bob", "charlie", "Check out the examples directory for usage patterns", 
                 &mut messages, &mut users, &mut user_message_indices)?;
    
    // Display all users and their message counts
    println!("\n👥 Users:");
    let mut all_users = users.iter()?;
    all_users.sort_by_key(|(username, _)| username.clone());
    
    for (username, user) in all_users {
        println!("  {} ({}) - {} messages sent", 
                 user.display_name, username, user.message_count);
    }
    
    // Display all messages
    println!("\n📨 All messages:");
    for (i, msg) in messages.iter()?.enumerate() {
        let msg = msg?;
        println!("  [{}] {} → {}: {}", i, msg.from, msg.to, msg.content);
    }
    
    // Show inbox for each user
    println!("\n📥 User inboxes:");
    for (username, _) in users.iter()? {
        if let Some(indices) = user_message_indices.get(&username)? {
            println!("\n  {}'s inbox ({} messages):", username, indices.len());
            for &idx in &indices {
                if let Some(msg) = messages.get(idx)? {
                    println!("    From {}: {}", msg.from, msg.content);
                }
            }
        }
    }
    
    // Statistics
    println!("\n📊 Statistics:");
    println!("  Total users: {}", users.len()?);
    println!("  Total messages: {}", messages.len()?);
    
    // Demonstrate persistence
    println!("\n💾 Data has been persisted to disk!");
    println!("  Database location: ./chat_db");
    
    // Clean up
    drop(messages);
    drop(users);
    drop(user_message_indices);
    drop(db);
    
    // Remove the database for this example
    std::fs::remove_dir_all("chat_db").ok();
    
    println!("\n✅ Example completed!");
    
    Ok(())
} 