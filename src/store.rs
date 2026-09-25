//! `cores.json` — the bot's owner and their cores, keys included. Written
//! owner-only (0600 inside a 0700 data dir): the keys are the file's whole
//! point, and on a server there is no DPAPI to seal them with.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// No `Debug`: `key` is a MoonProto key export.
#[derive(Clone, Serialize, Deserialize)]
pub struct CoreEntry {
    pub id: String,
    pub name: String,
    pub key: String,
    /// Overrides for a key that carries no endpoint; empty / 0 → the key's.
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub port: u16,
    /// The core's base currency as it last reported it — reports read it
    /// while the core is offline too.
    #[serde(default)]
    pub currency: String,
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReportFormat {
    #[default]
    Image,
    Text,
}

/// The owner's choices from the Settings screen.
#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    /// Emulator trades as their own rows in the reports.
    pub show_emulator: bool,
    pub report_format: ReportFormat,
}

impl Default for Settings {
    fn default() -> Self {
        Self { show_emulator: true, report_format: ReportFormat::Image }
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct StoreFile {
    /// The Telegram user the bot answers; `None` until claimed.
    #[serde(default)]
    owner: Option<i64>,
    #[serde(default)]
    cores: Vec<CoreEntry>,
    #[serde(default)]
    settings: Settings,
}

pub struct Store {
    path: PathBuf,
    data: Mutex<StoreFile>,
}

impl Store {
    pub fn load(path: PathBuf) -> Result<Self, String> {
        let data = match fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str::<StoreFile>(&text)
                .map_err(|e| format!("{} is not valid: {e}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => StoreFile::default(),
            Err(e) => return Err(format!("could not read {}: {e}", path.display())),
        };
        Ok(Self { path, data: Mutex::new(data) })
    }

    pub fn owner(&self) -> Option<i64> {
        self.data.lock().ok().and_then(|d| d.owner)
    }

    pub fn set_owner(&self, owner: i64) -> Result<(), String> {
        self.edit(|d| d.owner = Some(owner))
    }

    pub fn settings(&self) -> Settings {
        self.data.lock().map(|d| d.settings).unwrap_or_default()
    }

    pub fn set_show_emulator(&self, show: bool) -> Result<(), String> {
        self.edit(|d| d.settings.show_emulator = show)
    }

    pub fn set_report_format(&self, format: ReportFormat) -> Result<(), String> {
        self.edit(|d| d.settings.report_format = format)
    }

    pub fn all(&self) -> Vec<CoreEntry> {
        self.data.lock().map(|d| d.cores.clone()).unwrap_or_default()
    }

    pub fn get(&self, id: &str) -> Option<CoreEntry> {
        self.all().into_iter().find(|c| c.id == id)
    }

    pub fn add(&self, entry: CoreEntry) -> Result<(), String> {
        self.edit(|d| d.cores.push(entry))
    }

    pub fn remove(&self, id: &str) -> Result<(), String> {
        self.edit(|d| d.cores.retain(|c| c.id != id))
    }

    /// Saved only when it differs — the core reports it on every connect.
    pub fn set_currency(&self, id: &str, currency: &str) -> Result<(), String> {
        let unchanged = self
            .data
            .lock()
            .map(|d| d.cores.iter().any(|e| e.id == id && e.currency == currency))
            .unwrap_or(true);
        if unchanged {
            return Ok(());
        }
        self.edit(|d| {
            if let Some(e) = d.cores.iter_mut().find(|e| e.id == id) {
                e.currency = currency.to_string();
            }
        })
    }

    fn edit(&self, f: impl FnOnce(&mut StoreFile)) -> Result<(), String> {
        let mut data = self.data.lock().map_err(|_| "store is poisoned".to_string())?;
        let mut next = data.clone();
        f(&mut next);
        write_private(&self.path, &serde_json::to_vec_pretty(&next).map_err(|e| e.to_string())?)?;
        *data = next;
        Ok(())
    }
}

/// Write through a temp file and rename, so a crash never leaves half a
/// file — created owner-only, never widened after the fact.
fn write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let tmp = path.with_extension("json.tmp");
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(&tmp).map_err(|e| format!("could not write {}: {e}", tmp.display()))?;
    file.write_all(bytes)
        .and_then(|_| file.sync_all())
        .map_err(|e| format!("could not write {}: {e}", tmp.display()))?;
    fs::rename(&tmp, path).map_err(|e| format!("could not replace {}: {e}", path.display()))
}

/// The data dir, owner-only.
pub fn ensure_private_dir(dir: &Path) -> Result<(), String> {
    fs::create_dir_all(dir).map_err(|e| format!("could not create {}: {e}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
            .map_err(|e| format!("could not restrict {}: {e}", dir.display()))?;
    }
    Ok(())
}
