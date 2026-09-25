//! Telegram bot reporting realized PnL (today / this month) from MoonBot
//! cores over MoonProto. One instance per user, on their own bot token: the
//! owner claims it once from the chat, then adds their cores by key; the bot
//! keeps a session to every core and a local replica of its `Orders` report.

mod bot;
mod config;
mod cores;
mod lock;
mod pnl;
mod replica;
mod status;
mod store;
mod table;
mod telegram;

use std::sync::Arc;

use config::Config;

#[tokio::main]
async fn main() {
    let _ = dotenvy::dotenv();
    if std::env::args().nth(1).as_deref() == Some("passphrase") {
        if let Err(e) = passphrase_command() {
            eprintln!("{e}");
            std::process::exit(1);
        }
        return;
    }
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info,moonproto=warn")).init();
    if let Err(e) = run().await {
        log::error!("{e}");
        status::error(&config::data_dir(), &e);
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
    let link = claim_code.as_ref().map(|code| format!("https://t.me/{username}?start={code}"));
    let status = status::Status::new(&cfg.data_dir, link);
    let cores = cores::Cores::new(cfg.data_dir.join("reports"), Arc::clone(&store));
    // Locked, the cores connect once the owner sends the passphrase.
    let locked = store.is_locked();
    if !locked {
        cores.connect_all();
    }
    cores.supervise();

    let bot = bot::Bot::new(tg, claim_code, cfg.report_offset_min, Arc::clone(&store), Arc::clone(&cores), status.clone());
    let note = if locked { " — locked until the owner sends the passphrase" } else { "" };
    log::info!("@{username} started with {} core(s){note}", store.all().len());
    status.update(&store);
    tokio::select! {
        _ = bot.run() => {}
        _ = shutdown_signal() => log::info!("shutting down"),
    }
    tokio::task::spawn_blocking(move || cores.shutdown()).await.map_err(|e| e.to_string())?;
    status.stopped();
    Ok(())
}

/// `pnl-bot passphrase`: set, change or remove the lock on the core keys at
/// the terminal. Run with the service stopped — it would write its own copy
/// of `cores.json` back.
fn passphrase_command() -> Result<(), String> {
    let dir = config::data_dir();
    store::ensure_private_dir(&dir)?;
    let store = store::Store::load(dir.join("cores.json"))?;
    let had_lock = store.has_lock();
    if had_lock && !store.unlock(&ask("Current passphrase: ")?)? {
        return Err("Wrong passphrase.".to_string());
    }
    let empty = if had_lock { "Enter removes the lock" } else { "Enter skips" };
    loop {
        let new = ask(&format!("New passphrase ({empty}): "))?;
        if new.is_empty() {
            if had_lock {
                store.set_passphrase(None)?;
                println!("Lock removed: the core keys are stored unencrypted again.");
            } else {
                println!("Skipped: the core keys are stored unencrypted.");
            }
            return Ok(());
        }
        if new.chars().count() < lock::MIN_CHARS {
            println!("Use at least {} characters.", lock::MIN_CHARS);
            continue;
        }
        if *ask("Repeat it: ")? != *new {
            println!("They don't match. Try again.");
            continue;
        }
        store.set_passphrase(Some(&new))?;
        println!("Locked. After every restart, send this passphrase to the bot in Telegram to unlock it.");
        return Ok(());
    }
}

/// A line from the terminal, not echoed; trimmed, as Telegram's are.
fn ask(prompt: &str) -> Result<zeroize::Zeroizing<String>, String> {
    let line = zeroize::Zeroizing::new(rpassword::prompt_password(prompt).map_err(|e| format!("could not read it: {e}"))?);
    Ok(zeroize::Zeroizing::new(line.trim().to_string()))
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
