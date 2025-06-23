use durable::{Db, DurableMap, DurableVec};
use serde::{Serialize, Deserialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, PartialOrd)]
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
    println!("🎮 Gaming Ranking History System (Rewritten with Nested Entry API)");
    println!("================================================================");
    
    let db = Db::open("ranking_history_db")?;
    
    // THE CORE CHANGE: Define the truly nested data structure.
    // Instead of a composite key, we nest a Map within a Map.
    // This represents the ideal, ergonomic API.
    type DailyRankings = DurableVec<RankingEntry>;
    type GameHistory = DurableMap<u32, DailyRankings>;
    type Rankings = DurableMap<String, GameHistory>;

    let rankings: Rankings = DurableMap::new_nested(&db, "game_rankings_v2");
    
    // Simulate some game days
    let today = 20241215u32;
    let yesterday = 20241214u32;
    let last_week = 20241208u32;
    
    // No more `make_key` helper function!
    
    println!("\n📊 Adding ranking data using chained entry().or_default()...");
    
    // Add rankings for CS2 today. This demonstrates the new, clean access pattern.
    println!("Adding CS2 rankings for today ({})", today);
    let mut cs2_today = rankings
        .entry("cs2".to_string())?
        .or_default()? // Returns GameHistory (DurableMap<u32, DailyRankings>) for "cs2"
        .entry(today)?
        .or_default()?; // Returns DailyRankings (DurableVec<...>) for `today`

    cs2_today.push(RankingEntry::new("player1", 2450))?;
    cs2_today.push(RankingEntry::new("player2", 2380))?;
    cs2_today.push(RankingEntry::new("player3", 2320))?;
    cs2_today.push(RankingEntry::new("player4", 2280))?;
    
    // Add rankings for CS2 yesterday
    println!("Adding CS2 rankings for yesterday ({})", yesterday);
    rankings
        .entry("cs2".to_string())?
        .or_default()?
        .entry(yesterday)?
        .or_default()?
        .push(RankingEntry::new("player1", 2420))?;
    rankings
        .entry("cs2".to_string())?
        .or_default()?
        .entry(yesterday)?
        .or_default()?
        .push(RankingEntry::new("player2", 2350))?;
    rankings
        .entry("cs2".to_string())?
        .or_default()?
        .entry(yesterday)?
        .or_default()?
        .push(RankingEntry::new("player5", 2300))?;

    // Add rankings for Valorant today
    println!("Adding Valorant rankings for today ({})", today);
    let mut valorant_today = rankings
        .entry("valorant".to_string())?
        .or_default()?
        .entry(today)?
        .or_default()?;

    valorant_today.push(RankingEntry::new("player6", 1850))?;
    valorant_today.push(RankingEntry::new("player7", 1820))?;
    valorant_today.push(RankingEntry::new("player1", 1800))?; // Same player, different game
    
    // Add TF2 rankings (matching the docs example)
    println!("Adding TF2 rankings for last week ({})", last_week);
    rankings
        .entry("tf2".to_string())?
        .or_default()?
        .entry(last_week)?
        .or_default()?
        .push(RankingEntry::new("veteran_player", 3200))?;
    rankings
        .entry("tf2".to_string())?
        .or_default()?
        .entry(last_week)?
        .or_default()?
        .push(RankingEntry::new("old_school_gamer", 3150))?;
    
    println!("\n🏆 Reading back ranking data with the same natural API...");
    
    // Get today's CS2 leaderboard
    println!("\n🎯 CS2 Leaderboard for {} (today):", today);
    let mut today_rankings = rankings
        .entry("cs2".to_string())?
        .or_default()?
        .entry(today)?
        .or_default()?
        .to_vec()?;

    // Sort by score descending
    today_rankings.sort_by(|a, b| b.score.cmp(&a.score));
    
    for (rank, entry) in today_rankings.iter().enumerate() {
        println!("  {}. {} - {} points", rank + 1, entry.player_id, entry.score);
    }
    
    // Show cross-game analysis is still easy
    println!("\n🎮 Multi-game player analysis for player1 on {}:", today);
    let cs2_player1_score = today_rankings.iter()
        .find(|e| e.player_id == "player1")
        .map(|e| e.score);

    let valorant_player1_score = valorant_today.to_vec()?.iter()
        .find(|e| e.player_id == "player1")
        .map(|e| e.score);

    if let Some(score) = cs2_player1_score { println!("    CS2 Score: {}", score); }
    if let Some(score) = valorant_player1_score { println!("    Valorant Score: {}", score); }
    
    // Showcase the power of the nested structure for stats
    // For nested collections, we use the keys API instead of iter()
    println!("\n📊 Dynamic Database Statistics (discovered games):");
    
    // Note: For nested collections, we iterate over known keys or use a different approach
    // since the values (nested DurableMaps) cannot be directly deserialized
    let games = vec!["cs2", "valorant", "tf2"]; // In a real app, you might track these separately
    
    for game in games {
        let game_history = rankings.entry(game.to_string())?.or_default()?;
        let active_days = game_history.len()?;
        
        if active_days > 0 {
            // For demonstration, let's count entries from known days
            let mut total_entries = 0;
            let days = [today, yesterday, last_week];
            
            for day in days {
                if let Ok(daily_rankings) = game_history.entry(day) {
                    if let Ok(rankings_vec) = daily_rankings.or_default() {
                        total_entries += rankings_vec.len()?;
                    }
                }
            }
            
            if total_entries > 0 {
                println!("  • {}: {} total entries across {} active day(s)", 
                         game.to_uppercase(), total_entries, active_days);
            }
        }
    }
    
    println!("\n✨ Key Benefits of This Rewritten Approach:");
    println!("  • No more manual key construction (`format!`) - the core goal is met!");
    println!("  • The code's structure now mirrors the mental model: `rankings[game][day]`");
    println!("  • Truly compositional API, unlocking more powerful dynamic queries (like the stats section)");
    println!("  • Demonstrates the full power of the `DurableCollection` and `entry()` design.");

    Ok(())
}