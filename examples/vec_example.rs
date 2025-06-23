use durable::{Db, DurableVec};
use serde::{Serialize, Deserialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct Task {
    id: u64,
    title: String,
    completed: bool,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Open or create a database
    let db = Db::open("example_db")?;
    
    // Create a persistent vector of tasks
    let mut tasks = DurableVec::<Task>::new(&db, "tasks")?;
    
    // Add some tasks
    tasks.push(Task {
        id: 1,
        title: "Build Durable library".to_string(),
        completed: true,
    })?;
    
    tasks.push(Task {
        id: 2,
        title: "Write comprehensive tests".to_string(),
        completed: true,
    })?;
    
    tasks.push(Task {
        id: 3,
        title: "Create documentation".to_string(),
        completed: false,
    })?;
    
    println!("Total tasks: {}", tasks.len()?);
    
    // Iterate through all tasks
    println!("\nAll tasks:");
    for (i, task) in tasks.iter()?.enumerate() {
        let task = task?;
        println!("  [{}] {} - {}", 
            i, 
            task.title, 
            if task.completed { "✓" } else { "○" }
        );
    }
    
    // Get a specific task
    if let Some(task) = tasks.get(1)? {
        println!("\nTask at index 1: {:?}", task);
    }
    
    // Mark the last task as completed
    if let Some(mut last_task) = tasks.pop()? {
        println!("\nCompleting task: {}", last_task.title);
        last_task.completed = true;
        tasks.push(last_task)?;
    }
    
    // The data persists even after the program exits!
    println!("\nData has been persisted to disk.");
    
    Ok(())
} 