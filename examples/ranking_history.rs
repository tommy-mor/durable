use durable::{Db, DurableMap, DurableVec};
use serde::{Serialize, Deserialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RankingEntry {
    player_id: String,
    score: i32,
    timestamp: u64,
}

impl RankingEntry {
    fn new(player_id: &str, score: i32) -> Self {
        Self {
            player_id: player_id.to_string(),
            score,
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("🎮 Gaming Ranking History System");
    println!("================================");
    
    let db = Db::open("ranking_history_db")?;
    
    // Create the nested structure: (Game Mode + Day) → List of (Player, Score)
    // We use a composite key since deep nesting (Map->Map->Vec) requires DurableMap serialization
    // Key format: "game_mode:day" -> Vec<RankingEntry>
    type RankingHistory = DurableMap<String, DurableVec<RankingEntry>>;
    let rankings: RankingHistory = DurableMap::new_nested(&db, "game_rankings");
    
    // Simulate some game days
    let today = 20241215u32;
    let yesterday = 20241214u32;
    let last_week = 20241208u32;
    
    // Helper function to create composite keys
    let make_key = |game: &str, day: u32| format!("{}:{}", game, day);
    
    println!("\n📊 Adding ranking data...");
    
    // Add rankings for different game modes and days
    
    // CS2 rankings for today
    println!("Adding CS2 rankings for today ({})", today);
    let mut cs2_today = rankings.entry(make_key("cs2", today))?.or_default()?;
    cs2_today.push(RankingEntry::new("player1", 2450))?;
    cs2_today.push(RankingEntry::new("player2", 2380))?;
    cs2_today.push(RankingEntry::new("player3", 2320))?;
    cs2_today.push(RankingEntry::new("player4", 2280))?;
    
    // CS2 rankings for yesterday
    println!("Adding CS2 rankings for yesterday ({})", yesterday);
    let mut cs2_yesterday = rankings.entry(make_key("cs2", yesterday))?.or_default()?;
    cs2_yesterday.push(RankingEntry::new("player1", 2420))?;
    cs2_yesterday.push(RankingEntry::new("player2", 2350))?;
    cs2_yesterday.push(RankingEntry::new("player5", 2300))?;
    
    // Valorant rankings for today
    println!("Adding Valorant rankings for today ({})", today);
    let mut valorant_today = rankings.entry(make_key("valorant", today))?.or_default()?;
    valorant_today.push(RankingEntry::new("player6", 1850))?;
    valorant_today.push(RankingEntry::new("player7", 1820))?;
    valorant_today.push(RankingEntry::new("player1", 1800))?; // Same player, different game
    
    // TF2 rankings (matching the docs example)
    println!("Adding TF2 rankings for last week ({})", last_week);
    let mut tf2_lastweek = rankings.entry(make_key("tf2", last_week))?.or_default()?;
    tf2_lastweek.push(RankingEntry::new("veteran_player", 3200))?;
    tf2_lastweek.push(RankingEntry::new("old_school_gamer", 3150))?;
    
    println!("\n🏆 Reading back ranking data...");
    
    // Demonstrate natural access patterns - adapted for our composite key approach
    
    // Get today's CS2 leaderboard
    println!("\n🎯 CS2 Leaderboard for {} (today):", today);
    let cs2_today_read = rankings.entry(make_key("cs2", today))?.or_default()?;
    
    let mut today_rankings = Vec::new();
    for i in 0..cs2_today_read.len()? {
        if let Some(entry) = cs2_today_read.get(i)? {
            today_rankings.push(entry);
        }
    }
    // Sort by score descending
    today_rankings.sort_by(|a, b| b.score.cmp(&a.score));
    
    for (rank, entry) in today_rankings.iter().enumerate() {
        println!("  {}. {} - {} points", rank + 1, entry.player_id, entry.score);
    }
    
    // Compare with yesterday's performance
    println!("\n📈 CS2 Leaderboard for {} (yesterday):", yesterday);
    let cs2_yesterday_read = rankings.entry(make_key("cs2", yesterday))?.or_default()?;
    
    let mut yesterday_rankings = Vec::new();
    for i in 0..cs2_yesterday_read.len()? {
        if let Some(entry) = cs2_yesterday_read.get(i)? {
            yesterday_rankings.push(entry);
        }
    }
    yesterday_rankings.sort_by(|a, b| b.score.cmp(&a.score));
    
    for (rank, entry) in yesterday_rankings.iter().enumerate() {
        println!("  {}. {} - {} points", rank + 1, entry.player_id, entry.score);
    }
    
    // Show cross-game analysis
    println!("\n🎮 Multi-game player analysis:");
    
    // Check if player1 plays multiple games
    let cs2_player1_today = today_rankings.iter()
        .find(|e| e.player_id == "player1")
        .map(|e| e.score);
    
    let valorant_today_read = rankings.entry(make_key("valorant", today))?.or_default()?;
    let mut valorant_rankings = Vec::new();
    for i in 0..valorant_today_read.len()? {
        if let Some(entry) = valorant_today_read.get(i)? {
            valorant_rankings.push(entry);
        }
    }
    let valorant_player1_today = valorant_rankings.iter()
        .find(|e| e.player_id == "player1")
        .map(|e| e.score);
    
    if let (Some(cs2_score), Some(val_score)) = (cs2_player1_today, valorant_player1_today) {
        println!("  player1 is multi-talented:");
        println!("    CS2: {} points", cs2_score);
        println!("    Valorant: {} points", val_score);
    }
    
    // Show the power of the nested structure
    println!("\n📊 Database Statistics:");
    
    // Count total games
    let game_modes = ["cs2", "valorant", "tf2"];
    for game in &game_modes {
        // Check specific days we know have data
        let days_to_check = [today, yesterday, last_week];
        let mut total_players = 0;
        let mut active_days = 0;
        
        for day in &days_to_check {
            let day_rankings = rankings.entry(make_key(game, *day))?.or_default()?;
            let day_player_count = day_rankings.len()?;
            if day_player_count > 0 {
                total_players += day_player_count;
                active_days += 1;
            }
        }
        
        if total_players > 0 {
            println!("  {}: {} total player entries across {} active days", 
                     game.to_uppercase(), total_players, active_days);
        }
    }
    
    println!("\n✨ Key Benefits Demonstrated:");
    println!("  • Natural nested access: rankings[composite_key] -> Vec<RankingEntry>");
    println!("  • No complex SQL joins - just direct key-value access");
    println!("  • Type-safe access at every level");
    println!("  • Automatic collection creation with or_default()");
    println!("  • Full persistence - restart the program to see data persist!");
    println!("  • Atomic operations - each ranking update is crash-safe");
    println!("  • Efficient streaming - can iterate without loading entire datasets");
    
    Ok(())
}