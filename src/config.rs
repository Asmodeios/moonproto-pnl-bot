//! Settings from the environment (or a `.env` beside the binary).

use std::path::PathBuf;

/// No `Debug`: it holds the bot token.
pub struct Config {
    pub bot_token: String,
    /// Sets the owner up front instead of claiming the bot from the chat.
    pub owner_id: Option<i64>,
    pub data_dir: PathBuf,
    /// The cores' report clock against UTC, in minutes.
    pub report_offset_min: i64,
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        let bot_token = var("BOT_TOKEN").ok_or("BOT_TOKEN is not set")?;
        let owner_id = match var("OWNER_ID") {
            Some(v) => Some(v.parse::<i64>().map_err(|_| format!("OWNER_ID: `{v}` is not a Telegram user id"))?),
            None => None,
        };
        let data_dir = PathBuf::from(var("DATA_DIR").unwrap_or_else(|| "data".to_string()));
        let report_offset_min = match var("REPORT_UTC_OFFSET_MINUTES") {
            Some(v) => v
                .parse::<i64>()
                .ok()
                .filter(|m| m.abs() <= 14 * 60)
                .ok_or("REPORT_UTC_OFFSET_MINUTES must be minutes between -840 and 840")?,
            None => 0,
        };
        Ok(Self { bot_token, owner_id, data_dir, report_offset_min })
    }
}

fn var(name: &str) -> Option<String> {
    std::env::var(name).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}
