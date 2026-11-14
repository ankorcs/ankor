//! The interface between the agent and the game containers it runs.

pub mod constants;
pub mod schemas;
pub mod types;

/// The engine's backup file after `rounds_played` rounds.
#[must_use]
pub fn backup_file_name(rounds_played: u32) -> String {
    format!("ankor_round{rounds_played:02}.txt")
}
