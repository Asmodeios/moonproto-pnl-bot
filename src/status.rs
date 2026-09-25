//! `status` in the data dir: where the service stands, for `install.sh` and
//! `pnl --link` to read instead of parsing its log. One `key=value` a line:
//! `state=` unclaimed | locked | running | stopped | error, then `link=` and
//! `locked=yes|no` while unclaimed, or `error=` on an error.

use std::path::{Path, PathBuf};

use crate::store::{write_private, Store};

#[derive(Clone)]
pub struct Status {
    path: PathBuf,
    /// The claim link, while the bot has no owner.
    link: Option<String>,
}

impl Status {
    pub fn new(data_dir: &Path, link: Option<String>) -> Self {
        Self { path: path(data_dir), link }
    }

    /// Unclaimed while the store has no owner, then locked until unlocked.
    pub fn update(&self, store: &Store) {
        let locked = store.is_locked();
        let text = match (store.owner(), &self.link) {
            (None, Some(link)) => format!("state=unclaimed\nlink={link}\nlocked={}\n", if locked { "yes" } else { "no" }),
            _ if locked => "state=locked\n".to_string(),
            _ => "state=running\n".to_string(),
        };
        write(&self.path, &text);
    }

    pub fn stopped(&self) {
        write(&self.path, "state=stopped\n");
    }
}

/// The service stopped on `error`.
pub fn error(data_dir: &Path, error: &str) {
    write(&path(data_dir), &format!("state=error\nerror={}\n", error.replace('\n', " ")));
}

fn path(data_dir: &Path) -> PathBuf {
    data_dir.join("status")
}

fn write(path: &Path, text: &str) {
    if let Err(e) = write_private(path, text.as_bytes()) {
        log::warn!("{e}");
    }
}
