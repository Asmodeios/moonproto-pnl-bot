//! Telegram bot reporting realized PnL (today / this month) from MoonBot
//! cores over MoonProto. One instance per user, on their own bot token: the
//! owner claims it once from the chat, then adds their cores by key; the bot
//! keeps a session to every core and a local replica of its `Orders` report.

mod bot;
mod config;
mod cores;
mod pnl;
mod replica;
mod store;
mod table;
mod telegram;

use std::sync::Arc;

use config::Config;

#[tokio::main]
async fn main() {
    let _ = dotenvy::dotenv();
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info,moonproto=warn")).init();
    if let Err(e) = run().await {
        log::error!("{e}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    let cfg = Config::from_env()?;
    store::ensure_private_dir(&cfg.data_dir)?;
    let store = Arc::new(store::Store::load(cfg.data_dir.join("cores.json"))?);
    if let Some(owner) = cfg.owner_id.filter(|id| store.owner() != Some(*id)) {
        store.set_owner(owner)?;
    }
    let tg = telegram::Tg::new(&cfg.bot_token);
    // Also proves the token before anything else starts.
    let me = tg.get_me().await.map_err(|e| format!("the bot token was not accepted: {e}"))?;
    let username = me.username.unwrap_or_default();
    let claim_code = match store.owner() {
        Some(_) => None,
        None => {
            let code = claim_code()?;
            log::warn!("this bot has no owner yet — claim code: {code}");
            log::warn!("open https://t.me/{username}?start={code} or send \"/start {code}\" to @{username}");
            Some(code)
        }
    };
    let cores = cores::Cores::new(cfg.data_dir.join("reports"), Arc::clone(&store));
    for entry in store.all() {
        if let Err(e) = cores.connect(&entry) {
            log::error!("[core {}] {e}", entry.id);
        }
    }
    cores.supervise();

    let bot = bot::Bot::new(tg, claim_code, cfg.report_offset_min, Arc::clone(&store), Arc::clone(&cores));
    log::info!("@{username} started with {} core(s)", store.all().len());
    tokio::select! {
        _ = bot.run() => {}
        _ = shutdown_signal() => log::info!("shutting down"),
    }
    tokio::task::spawn_blocking(move || cores.shutdown()).await.map_err(|e| e.to_string())
}

/// 12 characters of `[a-z0-9]` (~62 bits): unguessable over the Bot API's
/// rate, and valid as a `t.me/<bot>?start=` deep-link parameter.
fn claim_code() -> Result<String, String> {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut bytes = [0u8; 12];
    getrandom::getrandom(&mut bytes).map_err(|e| format!("no randomness for the claim code: {e}"))?;
    Ok(bytes.iter().map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char).collect())
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
