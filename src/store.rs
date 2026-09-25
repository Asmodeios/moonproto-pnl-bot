//! `cores.json` — the bot's owner and their cores, keys included. Written
//! owner-only (0600 inside a 0700 data dir). With a passphrase lock the keys
//! are sealed on disk (`lock`) and held in the clear only in memory, from the
//! owner's unlock on; until then the store is locked and has no keys.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::lock::{LockFile, Sealer};

/// No `Debug`: `key` is a MoonProto key export.
#[derive(Clone, Serialize, Deserialize)]
pub struct CoreEntry {
    pub id: String,
    pub name: String,
    /// Empty while the store is locked, and on disk under a lock.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub key: String,
    /// `key` sealed, on disk under a lock.
    #[serde(default, rename = "sealedKey", skip_serializing_if = "String::is_empty")]
    pub sealed_key: String,
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
    Image,
    #[default]
    Text,
}

/// The owner's choices from the Settings screen.
#[derive(Clone, Copy, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    /// Emulator trades as their own rows in the reports.
    pub show_emulator: bool,
    pub report_format: ReportFormat,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    lock: Option<LockFile>,
}

pub struct Store {
    path: PathBuf,
    data: Mutex<StoreFile>,
    /// The key from the passphrase, once the owner has unlocked the store.
    sealer: Mutex<Option<Sealer>>,
}

impl Store {
    pub fn load(path: PathBuf) -> Result<Self, String> {
        let data = match fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str::<StoreFile>(&text)
                .map_err(|e| format!("{} is not valid: {e}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => StoreFile::default(),
            Err(e) => return Err(format!("could not read {}: {e}", path.display())),
        };
        Ok(Self { path, data: Mutex::new(data), sealer: Mutex::new(None) })
    }

    /// Under a passphrase lock, unlocked or not.
    pub fn has_lock(&self) -> bool {
        self.data.lock().is_ok_and(|d| d.lock.is_some())
    }

    /// Waiting for the passphrase: the cores' keys are not known yet.
    pub fn is_locked(&self) -> bool {
        self.has_lock() && self.sealer.lock().is_ok_and(|s| s.is_none())
    }

    /// Unseal every core's key with `passphrase`; `false` for a wrong one.
    /// Takes Argon2's half a second.
    pub fn unlock(&self, passphrase: &str) -> Result<bool, String> {
        let mut data = self.data.lock().map_err(poisoned)?;
        let Some(lock) = &data.lock else {
            return Ok(true);
        };
        let Some(sealer) = lock.open(passphrase)? else {
            return Ok(false);
        };
        let mut cores = data.cores.clone();
        for core in cores.iter_mut().filter(|c| !c.sealed_key.is_empty()) {
            core.key = sealer
                .open(&core.sealed_key, core.id.as_bytes())
                .and_then(|k| String::from_utf8(k.to_vec()).map_err(|_| "is not text".to_string()))
                .map_err(|e| format!("the sealed key of core {}: {e}", core.id))?;
        }
        data.cores = cores;
        *self.sealer.lock().map_err(poisoned)? = Some(sealer);
        Ok(true)
    }

    /// Drop every core and the lock — the way out of a forgotten passphrase.
    /// Answers the ids dropped.
    pub fn forget(&self) -> Result<Vec<String>, String> {
        let ids = self.all().into_iter().map(|c| c.id).collect();
        self.edit(|d| {
            d.cores.clear();
            d.lock = None;
        })?;
        Ok(ids)
    }

    /// Seal the keys under a new passphrase, or with `None` store them in
    /// the clear again. Only while unlocked.
    pub fn set_passphrase(&self, passphrase: Option<&str>) -> Result<(), String> {
        let mut data = self.data.lock().map_err(poisoned)?;
        let mut sealer = self.sealer.lock().map_err(poisoned)?;
        if data.lock.is_some() && sealer.is_none() {
            return Err("the store is locked".to_string());
        }
        let mut next = data.clone();
        let new_sealer = match passphrase {
            Some(p) => {
                let (lock, sealer) = LockFile::new(p)?;
                next.lock = Some(lock);
                Some(sealer)
            }
            None => {
                next.lock = None;
                None
            }
        };
        self.write(&next, new_sealer.as_ref())?;
        *data = next;
        *sealer = new_sealer;
        Ok(())
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
        self.data.lock().ok()?.cores.iter().find(|c| c.id == id).cloned()
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
        let mut data = self.data.lock().map_err(poisoned)?;
        let mut next = data.clone();
        f(&mut next);
        let sealer = self.sealer.lock().map_err(poisoned)?;
        self.write(&next, sealer.as_ref())?;
        *data = next;
        Ok(())
    }

    /// `data` to disk, its keys sealed when it has a lock. A key still
    /// unknown (locked) keeps the sealed copy it was loaded with.
    fn write(&self, data: &StoreFile, sealer: Option<&Sealer>) -> Result<(), String> {
        let mut disk = data.clone();
        for core in &mut disk.cores {
            if core.key.is_empty() {
                continue;
            }
            match (&data.lock, sealer) {
                (None, _) => core.sealed_key.clear(),
                (Some(_), Some(sealer)) => {
                    core.sealed_key = sealer.seal(core.key.as_bytes(), core.id.as_bytes())?;
                    core.key.clear();
                }
                (Some(_), None) => return Err("the store is locked".to_string()),
            }
        }
        write_private(&self.path, &serde_json::to_vec_pretty(&disk).map_err(|e| e.to_string())?)
    }
}

fn poisoned<T>(_: T) -> String {
    "store is poisoned".to_string()
}

/// Write through a temp file and rename, so a crash never leaves half a
/// file — created owner-only, never widened after the fact.
pub fn write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locked_store_round_trip() {
        let dir = std::env::temp_dir().join(format!("pnl-bot-store-{}", std::process::id()));
        ensure_private_dir(&dir).unwrap();
        let path = dir.join("cores.json");
        let _ = fs::remove_file(&path);
        let entry = |id: &str, key: &str| CoreEntry {
            id: id.into(),
            name: "Bin".into(),
            key: key.into(),
            sealed_key: String::new(),
            host: String::new(),
            port: 0,
            currency: String::new(),
        };

        let store = Store::load(path.clone()).unwrap();
        store.add(entry("c1", "SECRET-ONE")).unwrap();
        store.set_passphrase(Some("correct horse")).unwrap();
        store.add(entry("c2", "SECRET-TWO")).unwrap();
        assert!(!fs::read_to_string(&path).unwrap().contains("SECRET"));

        let store = Store::load(path.clone()).unwrap();
        assert!(store.is_locked());
        assert!(store.all().iter().all(|c| c.key.is_empty()));
        // A write while locked keeps the sealed keys.
        store.set_owner(7).unwrap();
        assert!(!store.unlock("wrong horse").unwrap());
        assert!(store.unlock("correct horse").unwrap());
        assert!(!store.is_locked());
        assert_eq!(store.get("c2").unwrap().key, "SECRET-TWO");

        store.set_passphrase(None).unwrap();
        let store = Store::load(path.clone()).unwrap();
        assert!(!store.has_lock());
        assert_eq!(store.get("c1").unwrap().key, "SECRET-ONE");
        assert_eq!(store.owner(), Some(7));
        let _ = fs::remove_dir_all(&dir);
    }
}
