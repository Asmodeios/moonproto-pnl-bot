//! New releases: the bot looks for one on GitHub and, when the owner presses
//! Update, asks the server to install it. The bot runs unprivileged, so all
//! it does is leave a request file in its data dir; `pnl-bot-update.path`
//! (deploy/) sees it and runs `pnl --update` as root, which restarts the bot.

use std::path::Path;
use std::time::Duration;

use crate::store::write_private;

/// `repository` in Cargo.toml; `REPO` in deploy/pnl.sh.
pub const REPO_URL: &str = env!("CARGO_PKG_REPOSITORY");
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");
/// Installed by install.sh: the server takes the bot's update requests.
const PATH_UNIT: &str = "/etc/systemd/system/pnl-bot-update.path";
/// Watched by pnl-bot-update.path, whose service deletes it as it starts.
const REQUEST: &str = "update-request";
/// The end of a failed update's log, left by pnl-bot-update.service.
const FAILED: &str = "update-failed";
/// The version that ran last, to tell an update from a restart.
const LAST_RUN: &str = "version";

/// The newest release's version, without the `v`: from where
/// `/releases/latest` redirects, as `pnl --update` finds it.
pub async fn latest() -> Result<String, String> {
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| format!("release check: {e}"))?;
    let res = http
        .head(format!("{REPO_URL}/releases/latest"))
        .send()
        .await
        .map_err(|e| format!("release check: {e}"))?;
    let tag = res.url().path_segments().and_then(|mut s| s.next_back()).unwrap_or("");
    tag.strip_prefix('v')
        .filter(|v| parse(v).is_some())
        .map(str::to_string)
        .ok_or_else(|| format!("release check: no release tag at {}", res.url()))
}

/// Whether `version` is newer than this build. A pre-release never is.
pub fn newer(version: &str) -> bool {
    parse(version).is_some_and(|v| Some(v) > parse(CURRENT))
}

fn parse(version: &str) -> Option<(u64, u64, u64)> {
    let mut parts = version.split('.').map(|p| p.parse::<u64>().ok());
    let v = (parts.next()??, parts.next()??, parts.next()??);
    parts.next().is_none().then_some(v)
}

/// Whether the server installs updates the bot asks for.
pub fn can_request() -> bool {
    Path::new(PATH_UNIT).exists()
}

pub fn request(data_dir: &Path) -> Result<(), String> {
    let _ = std::fs::remove_file(data_dir.join(FAILED));
    write_private(&data_dir.join(REQUEST), b"")
}

/// Why the last update failed, once: the end of its log, maybe empty.
pub fn failure(data_dir: &Path) -> Option<String> {
    let path = data_dir.join(FAILED);
    let log = std::fs::read_to_string(&path).ok()?;
    let _ = std::fs::remove_file(&path);
    Some(log.trim().to_string())
}

/// Take back a request nothing on the server took; whether there was one.
pub fn withdraw(data_dir: &Path) -> bool {
    std::fs::remove_file(data_dir.join(REQUEST)).is_ok()
}

/// The version that ran before this one, once, on its first start — however
/// it was updated.
pub fn updated_from(data_dir: &Path) -> Option<String> {
    let path = data_dir.join(LAST_RUN);
    let last = std::fs::read_to_string(&path).ok().map(|v| v.trim().to_string());
    if last.as_deref() == Some(CURRENT) {
        return None;
    }
    if let Err(e) = write_private(&path, CURRENT.as_bytes()) {
        log::warn!("{e}");
    }
    last
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_by_number() {
        assert_eq!(parse("0.1.10"), Some((0, 1, 10)));
        assert_eq!(parse("0.2.0-rc1"), None);
        assert_eq!(parse("1.2"), None);
        assert_eq!(parse("1.2.3.4"), None);
        assert!(newer("999.0.0"));
        assert!(!newer(CURRENT));
        assert!(!newer("0.0.1"));
    }
}
