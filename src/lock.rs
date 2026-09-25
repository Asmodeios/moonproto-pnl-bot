//! The passphrase lock. Argon2id turns the owner's passphrase into a key and
//! XChaCha20-Poly1305 seals each core key with it. Neither the passphrase nor
//! the derived key is ever written: `cores.json` keeps the salt, the Argon2
//! cost and a sealed check value that tells a wrong passphrase from a right one.

use argon2::{Algorithm, Argon2, Params, Version};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

pub const MIN_CHARS: usize = 8;
/// Argon2id memory (KiB), passes and lanes: 64 MiB, ~0.5 s on a small VPS.
const COST: (u32, u32, u32) = if cfg!(test) { (8, 1, 1) } else { (64 * 1024, 3, 1) };
const CHECK: &[u8] = b"pnl-bot passphrase check";
const CHECK_AAD: &[u8] = b"check";
const NONCE_LEN: usize = 24;

/// What `cores.json` keeps of the lock.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LockFile {
    m_cost: u32,
    t_cost: u32,
    p_cost: u32,
    salt: String,
    check: String,
}

/// The key derived from the passphrase. No `Debug`; zeroed on drop.
pub struct Sealer(XChaCha20Poly1305);

impl LockFile {
    /// A new lock on `passphrase`, with its sealer.
    pub fn new(passphrase: &str) -> Result<(Self, Sealer), String> {
        let mut salt = [0u8; 16];
        getrandom::getrandom(&mut salt).map_err(|e| format!("no randomness for the salt: {e}"))?;
        let sealer = derive(passphrase, &salt, COST)?;
        let check = sealer.seal(CHECK, CHECK_AAD)?;
        let (m_cost, t_cost, p_cost) = COST;
        Ok((Self { m_cost, t_cost, p_cost, salt: to_hex(&salt), check }, sealer))
    }

    /// The sealer, or `None` for a wrong passphrase. Slow on purpose.
    pub fn open(&self, passphrase: &str) -> Result<Option<Sealer>, String> {
        let salt = from_hex(&self.salt).ok_or("the lock in cores.json is damaged")?;
        let sealer = derive(passphrase, &salt, (self.m_cost, self.t_cost, self.p_cost))?;
        let right = sealer.open(&self.check, CHECK_AAD).is_ok_and(|c| c.as_slice() == CHECK);
        Ok(right.then_some(sealer))
    }
}

impl Sealer {
    /// `plain` sealed under a fresh nonce, bound to `aad`, as hex.
    pub fn seal(&self, plain: &[u8], aad: &[u8]) -> Result<String, String> {
        let mut nonce = [0u8; NONCE_LEN];
        getrandom::getrandom(&mut nonce).map_err(|e| format!("no randomness for a nonce: {e}"))?;
        let sealed = self
            .0
            .encrypt(XNonce::from_slice(&nonce), Payload { msg: plain, aad })
            .map_err(|_| "could not encrypt".to_string())?;
        Ok(to_hex(&[&nonce[..], &sealed].concat()))
    }

    pub fn open(&self, sealed: &str, aad: &[u8]) -> Result<Zeroizing<Vec<u8>>, String> {
        let bytes = from_hex(sealed).filter(|b| b.len() > NONCE_LEN).ok_or("damaged")?;
        let (nonce, sealed) = bytes.split_at(NONCE_LEN);
        self.0
            .decrypt(XNonce::from_slice(nonce), Payload { msg: sealed, aad })
            .map(Zeroizing::new)
            .map_err(|_| "does not decrypt".to_string())
    }
}

fn derive(passphrase: &str, salt: &[u8], (m, t, p): (u32, u32, u32)) -> Result<Sealer, String> {
    let params = Params::new(m, t, p, Some(32)).map_err(|e| format!("bad Argon2 cost in cores.json: {e}"))?;
    let mut key = Zeroizing::new([0u8; 32]);
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password_into(passphrase.as_bytes(), salt, key.as_mut())
        .map_err(|e| format!("could not derive the key: {e}"))?;
    let cipher = XChaCha20Poly1305::new_from_slice(key.as_ref()).map_err(|e| e.to_string())?;
    Ok(Sealer(cipher))
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn from_hex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len()).step_by(2).map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn right_and_wrong_passphrases() {
        let (lock, sealer) = LockFile::new("correct horse").unwrap();
        let sealed = sealer.seal(b"key", b"c1").unwrap();
        assert!(lock.open("wrong horse").unwrap().is_none());
        let opened = lock.open("correct horse").unwrap().unwrap();
        assert_eq!(opened.open(&sealed, b"c1").unwrap().as_slice(), b"key");
        // Bound to its core: a key moved to another core's entry doesn't open.
        assert!(opened.open(&sealed, b"c2").is_err());
    }

    #[test]
    fn hex_round_trip() {
        assert_eq!(from_hex(&to_hex(&[0, 15, 255])), Some(vec![0, 15, 255]));
        assert_eq!(from_hex("abc"), None);
        assert_eq!(from_hex("zz"), None);
    }
}
