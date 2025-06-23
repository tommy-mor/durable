use durable::{Db, DurableMap, DurableVec};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("🏆 Simple Game Ranking Example");
    println!("Demonstrating the pattern from docs/motivation.md");
    println!("===============================================");
    
    let db = Db::open("simple_ranking_db")?;
    
    // This is the exact pattern from the docs: Game Mode → List of (Player, Score)
    // For simplicity, we're showing one day's data per game mode
    let rankings: DurableMap<String, DurableVec<(String, i32)>> = DurableMap::new_nested(&db, "rankings");
    
    println!("\n📊 Adding TF2 rankings (from the docs example)...");
    
    // This is the exact code pattern shown in docs/motivation.md
    let mut tf2_rankings = rankings.entry("tf2".to_string())?.or_default()?;
    tf2_rankings.push(("player1".to_string(), 1500))?;
    tf2_rankings.push(("player2".to_string(), 1400))?;
    tf2_rankings.push(("player3".to_string(), 1300))?;
    
    println!("✅ Added TF2 rankings using the docs pattern!");
    
    // Add some other games for comparison
    println!("\n📊 Adding CS2 rankings...");
    let mut cs2_rankings = rankings.entry("cs2".to_string())?.or_default()?;
    cs2_rankings.push(("pro_player".to_string(), 2500))?;
    cs2_rankings.push(("skilled_gamer".to_string(), 2200))?;
    
    println!("✅ Added CS2 rankings!");
    
    // Now read back the data
    println!("\n🏆 Current TF2 Leaderboard:");
    let tf2_data = rankings.entry("tf2".to_string())?.or_default()?;
    
    // Convert to vec and sort for display
    let mut tf2_leaderboard = tf2_data.to_vec()?;
    tf2_leaderboard.sort_by(|a, b| b.1.cmp(&a.1)); // Sort by score descending
    
    for (rank, (player, score)) in tf2_leaderboard.iter().enumerate() {
        println!("  {}. {} - {} points", rank + 1, player, score);
    }
    
    println!("\n🏆 Current CS2 Leaderboard:");
    let cs2_data = rankings.entry("cs2".to_string())?.or_default()?;
    
    let mut cs2_leaderboard = cs2_data.to_vec()?;
    cs2_leaderboard.sort_by(|a, b| b.1.cmp(&a.1));
    
    for (rank, (player, score)) in cs2_leaderboard.iter().enumerate() {
        println!("  {}. {} - {} points", rank + 1, player, score);
    }
    
    println!("\n📈 Database Statistics:");
    println!("  TF2 has {} players", tf2_data.len()?);
    println!("  CS2 has {} players", cs2_data.len()?);
    
    println!("\n✨ This demonstrates the exact pattern from docs/motivation.md:");
    println!("  rankings.entry(game_mode)?.or_default()?.push((player, score))?;");
    println!("  ");
    println!("  Compare this to the manual key construction required with raw KV stores:");
    println!("  let key = format!(\"leaderboard:{{}}:{{}}:player:{{}}\", game_mode, day, player_id);");
    println!("  db.insert(key.as_bytes(), score.to_le_bytes())?;");
    println!("  ");
    println!("  Durable provides the ergonomic, type-safe abstraction over RocksDB!");
    
    Ok(())
}