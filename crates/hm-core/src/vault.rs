//! At-rest encryption for stored secrets (M12a-1, 2026-11-15).
//!
//! Before this, the `secrets` table held API keys in plaintext: a `sqlite3`
//! query, a DB snapshot, or a `VACUUM INTO` copy handed out household
//! credentials. The management API never returned them, but at rest they were
//! readable by anything that could read `data/home-mate.db`.
//!
//! Shape (user decision 2026-11-15):
//!
//! - Key: `data/secret.key`, 32 bytes from the kernel CSPRNG, created on first
//!   use with mode 0600, never regenerated. Losing it makes existing sealed
//!   values unreadable — that is reported, never papered over.
//! - Cipher: ChaCha20-Poly1305 (AEAD). The stored value is
//!   `enc:v1:<base64(nonce ‖ ciphertext ‖ tag)>`, base64 standard with
//!   padding. The secret's NAME is the associated data, so a ciphertext
//!   cannot be moved between rows.
//! - Legacy rows (no `enc:v1:` prefix) stay readable exactly as written and
//!   are re-sealed the next time the value is written. No migration pass
//!   rewrites rows behind the user's back.
//!
//! Why not a KMS / OS keyring: the device has no TPM-backed key store in the
//! supported path and the deployment has no external secret service. A root-
//! readable key file is the honest ceiling of "encrypted at rest on this
//! device" — it protects backups, snapshots and casual DB reads, not an
//! attacker who already has root. That limit is written down rather than
//! implied (docs/contracts-v0.md, v0.26).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

pub const SEALED_PREFIX: &str = "enc:v1:";
pub const KEY_FILE_NAME: &str = "secret.key";
pub const KEY_LEN: usize = 32;
const NONCE_LEN: usize = 12;

#[derive(Debug)]
pub enum VaultError {
    /// The key file exists but is not exactly 32 bytes.
    BadKeyFile(String),
    /// Encrypt/decrypt refused by the AEAD (wrong key, wrong name, or a
    /// truncated/corrupted value).
    Seal(String),
    Io(String),
}

impl std::fmt::Display for VaultError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VaultError::BadKeyFile(e) => write!(f, "secret key file unusable: {e}"),
            VaultError::Seal(e) => write!(f, "secret seal/open failed: {e}"),
            VaultError::Io(e) => write!(f, "secret key i/o: {e}"),
        }
    }
}

/// The device's secret key plus the counters the console reports.
pub struct SecretVault {
    key: [u8; KEY_LEN],
    path: Option<PathBuf>,
    /// Sealed rows that could not be opened (missing/replaced key, corruption).
    /// Counted, not thrown: one bad row must not take down the config service.
    undecryptable: AtomicU64,
}

/// Overwrite sensitive key bytes with volatile zero writes followed by a
/// sequentially consistent compiler fence so dead-store elimination cannot
/// remove the wipe.
fn zeroize_bytes(buf: &mut [u8]) {
    for byte in buf.iter_mut() {
        // Safety: `byte` is a valid, aligned, exclusively borrowed `&mut u8`.
        unsafe { std::ptr::write_volatile(byte, 0) };
    }
    std::sync::atomic::compiler_fence(Ordering::SeqCst);
}

impl Drop for SecretVault {
    fn drop(&mut self) {
        zeroize_bytes(&mut self.key);
    }
}

impl SecretVault {
    /// Load `data/secret.key`, creating it (0600, exclusive) when absent.
    pub fn load_or_create(data_dir: &Path) -> Result<Self, VaultError> {
        let path = data_dir.join(KEY_FILE_NAME);
        // A key file that exists but has the wrong size is a hard error: the
        // alternative — overwriting it — would silently destroy every stored
        // secret the device still needs.
        if let Ok(mut bytes) = std::fs::read(&path) {
            if bytes.len() != KEY_LEN {
                let len = bytes.len();
                zeroize_bytes(&mut bytes);
                return Err(VaultError::BadKeyFile(format!(
                    "{} has {} bytes, expected {KEY_LEN}",
                    path.display(),
                    len
                )));
            }
            let mut key = [0u8; KEY_LEN];
            key.copy_from_slice(&bytes);
            zeroize_bytes(&mut bytes);
            tighten(&path);
            return Ok(Self {
                key,
                path: Some(path),
                undecryptable: AtomicU64::new(0),
            });
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| VaultError::Io(e.to_string()))?;
        }
        let mut key = random_key()?;
        // `create_new` is the race guard: two cores starting together must end
        // up with ONE key, so the loser re-reads the winner's file.
        let written = {
            use std::fs::OpenOptions;
            let mut opts = OpenOptions::new();
            opts.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            match opts.open(&path) {
                Ok(mut f) => {
                    if let Err(e) = f.write_all(&key).and_then(|_| f.sync_all()) {
                        zeroize_bytes(&mut key);
                        return Err(VaultError::Io(e.to_string()));
                    }
                    true
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => false,
                Err(e) => {
                    zeroize_bytes(&mut key);
                    return Err(VaultError::Io(e.to_string()));
                }
            }
        };
        if written {
            tighten(&path);
            return Ok(Self {
                key,
                path: Some(path),
                undecryptable: AtomicU64::new(0),
            });
        }
        zeroize_bytes(&mut key);
        Self::load_or_create(data_dir)
    }

    /// Test/in-memory key. Never touches the filesystem; a fresh one every
    /// call, so a store reopened in memory cannot read another's rows.
    pub fn ephemeral() -> Self {
        let key = random_key().unwrap_or_else(|_| {
            // random_key only fails when /dev/urandom is unreadable (fatal for
            // the device's own API tokens too); a test still gets a key.
            let mut k = [0u8; KEY_LEN];
            for (i, b) in k.iter_mut().enumerate() {
                *b = (std::process::id() as usize + i) as u8;
            }
            k
        });
        Self {
            key,
            path: None,
            undecryptable: AtomicU64::new(0),
        }
    }

    pub fn key_path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn undecryptable(&self) -> u64 {
        self.undecryptable.load(Ordering::Relaxed)
    }

    /// `enc:v1:<b64(nonce ‖ ct ‖ tag)>`, bound to `name`.
    pub fn seal(&self, name: &str, plaintext: &str) -> Result<String, VaultError> {
        use base64::Engine;
        use chacha20poly1305::aead::{Aead, KeyInit, Payload};
        let cipher = chacha20poly1305::ChaCha20Poly1305::new((&self.key).into());
        let nonce = random_bytes::<NONCE_LEN>()?;
        let ct = cipher
            .encrypt(
                (&nonce).into(),
                Payload {
                    msg: plaintext.as_bytes(),
                    aad: name.as_bytes(),
                },
            )
            .map_err(|e| VaultError::Seal(e.to_string()))?;
        let mut out = Vec::with_capacity(NONCE_LEN + ct.len());
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ct);
        Ok(format!(
            "{SEALED_PREFIX}{}",
            base64::engine::general_purpose::STANDARD.encode(out)
        ))
    }

    /// Decrypt a sealed value. A value without the prefix is returned as-is
    /// (rows written before 2026-11-15) — that is the documented read path,
    /// not a fallback that hides a failure.
    pub fn open(&self, name: &str, stored: &str) -> Result<String, VaultError> {
        use base64::Engine;
        use chacha20poly1305::aead::{Aead, KeyInit, Payload};
        let Some(body) = stored.strip_prefix(SEALED_PREFIX) else {
            return Ok(stored.to_string());
        };
        let raw = base64::engine::general_purpose::STANDARD
            .decode(body.trim())
            .map_err(|e| VaultError::Seal(format!("base64: {e}")))?;
        if raw.len() < NONCE_LEN + 16 {
            return Err(VaultError::Seal("payload too short".into()));
        }
        let (nonce, ct) = raw.split_at(NONCE_LEN);
        let cipher = chacha20poly1305::ChaCha20Poly1305::new((&self.key).into());
        let nonce: [u8; NONCE_LEN] = nonce
            .try_into()
            .map_err(|_| VaultError::Seal("nonce".into()))?;
        let pt = cipher
            .decrypt(
                (&nonce).into(),
                Payload {
                    msg: ct,
                    aad: name.as_bytes(),
                },
            )
            .map_err(|_| VaultError::Seal("authentication failed".into()))?;
        String::from_utf8(pt).map_err(|e| VaultError::Seal(format!("utf8: {e}")))
    }

    pub fn note_undecryptable(&self) {
        self.undecryptable.fetch_add(1, Ordering::Relaxed);
    }
}

/// Sealed values are opaque strings; this is what "is it encrypted" means to
/// a caller that only has the row (backups, diagnostics, tests).
pub fn is_sealed(value: &str) -> bool {
    value.starts_with(SEALED_PREFIX)
}

#[cfg(unix)]
fn tighten(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    // A key that was copied in with a wider mode is corrected, not trusted.
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn tighten(_path: &Path) {}

fn random_key() -> Result<[u8; KEY_LEN], VaultError> {
    random_bytes::<KEY_LEN>()
}

fn random_bytes<const N: usize>() -> Result<[u8; N], VaultError> {
    use std::io::Read;
    let mut bytes = [0u8; N];
    let mut f = std::fs::File::open("/dev/urandom").map_err(|e| VaultError::Io(e.to_string()))?;
    f.read_exact(&mut bytes)
        .map_err(|e| VaultError::Io(e.to_string()))?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "hm-vault-{tag}-{}-{}",
            std::process::id(),
            crate::id::now_ms()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn seal_open_roundtrip_and_shape() {
        let v = SecretVault::ephemeral();
        let sealed = v.seal("llm.api_key", "sk-test-123").unwrap();
        assert!(sealed.starts_with("enc:v1:"), "{sealed}");
        assert!(
            !sealed.contains("sk-test-123"),
            "plaintext must not survive"
        );
        assert_eq!(v.open("llm.api_key", &sealed).unwrap(), "sk-test-123");
        // Legacy plaintext rows still read (pre-v0.26 rows).
        assert_eq!(v.open("llm.api_key", "sk-plain").unwrap(), "sk-plain");
    }

    #[test]
    fn a_ciphertext_cannot_be_moved_to_another_secret_name() {
        let v = SecretVault::ephemeral();
        let sealed = v.seal("llm.api_key", "sk-test").unwrap();
        assert!(
            v.open("tts.api_key", &sealed).is_err(),
            "the name is associated data: swapping rows must fail loudly"
        );
    }

    #[test]
    fn another_key_cannot_open_it() {
        let a = SecretVault::ephemeral();
        let sealed = a.seal("llm.api_key", "sk-test").unwrap();
        let b = SecretVault::ephemeral();
        assert!(b.open("llm.api_key", &sealed).is_err());
    }

    #[test]
    fn key_file_is_32_bytes_mode_0600_and_stable_across_reopen() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("key");
        let v = SecretVault::load_or_create(&dir).unwrap();
        let sealed = v.seal("llm.api_key", "sk-stable").unwrap();
        let key_path = dir.join(KEY_FILE_NAME);
        let meta = std::fs::metadata(&key_path).unwrap();
        assert_eq!(meta.len(), KEY_LEN as u64);
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        // Reopening reads the SAME key: a restart must not lose stored secrets.
        let again = SecretVault::load_or_create(&dir).unwrap();
        assert_eq!(again.open("llm.api_key", &sealed).unwrap(), "sk-stable");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_wrong_size_key_file_is_refused_not_overwritten() {
        let dir = temp_dir("bad");
        std::fs::write(dir.join(KEY_FILE_NAME), b"short").unwrap();
        assert!(matches!(
            SecretVault::load_or_create(&dir),
            Err(VaultError::BadKeyFile(_))
        ));
        assert_eq!(
            std::fs::read(dir.join(KEY_FILE_NAME)).unwrap(),
            b"short",
            "the file must be left exactly as found"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn secret_vault_drop_zeroizes_key_material() {
        let mut buf = [0xA5u8; KEY_LEN];
        zeroize_bytes(&mut buf);
        assert_eq!(buf, [0u8; KEY_LEN]);

        let mut vault = std::mem::ManuallyDrop::new(SecretVault {
            key: [0x5Au8; KEY_LEN],
            path: None,
            undecryptable: AtomicU64::new(0),
        });
        // Safety: we drop the ManuallyDrop contents in place exactly once and
        // inspect the still-allocated backing key bytes before leaving scope.
        unsafe {
            std::ptr::drop_in_place(&mut *vault);
        }
        assert_eq!(
            vault.key, [0u8; KEY_LEN],
            "SecretVault::drop must wipe key bytes to zero"
        );
    }
}
