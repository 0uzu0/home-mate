//! SHA-256 and Ed25519. The public key is compiled into this binary; the
//! private key never is (keygen/sign read it from an operator-supplied path).

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::Path;

use crate::hexutil::{from_hex, to_hex};

/// Public key shipped in the repo (`deploy/release/release-pubkey.hex`).
///
/// A debug build may override it with `HM_RELEASE_PUBKEY_OVERRIDE` so host
/// tests can sign throwaway packages; release builds ignore the variable, so
/// the device's trust root is always the compiled-in key.
pub fn embedded_pubkey_hex() -> String {
    #[cfg(debug_assertions)]
    if let Ok(v) = std::env::var("HM_RELEASE_PUBKEY_OVERRIDE") {
        let v = v.trim().to_string();
        if v.len() == 64 && v.bytes().all(|b| b.is_ascii_hexdigit()) {
            return v;
        }
    }
    include_str!("../../../deploy/release/release-pubkey.hex")
        .trim()
        .to_string()
}

pub fn sha256_bytes(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    to_hex(&h.finalize())
}

pub fn sha256_file(path: &Path) -> Result<String, String> {
    let mut f = std::fs::File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let mut h = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f
            .read(&mut buf)
            .map_err(|e| format!("read {}: {e}", path.display()))?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(to_hex(&h.finalize()))
}

/// Short, human-checkable key identity: sha256(pubkey)[:16 hex].
pub fn fingerprint(pubkey_hex: &str) -> String {
    sha256_bytes(&from_hex(pubkey_hex).unwrap_or_default())[..16].to_string()
}

pub fn load_seed_hex(path: &Path) -> Result<[u8; 32], String> {
    let raw =
        std::fs::read_to_string(path).map_err(|e| format!("read key {}: {e}", path.display()))?;
    let bytes = from_hex(&raw).map_err(|e| format!("key {}: {e}", path.display()))?;
    if bytes.len() != 32 {
        return Err(format!(
            "key {} is {} bytes, expected a 32-byte Ed25519 seed",
            path.display(),
            bytes.len()
        ));
    }
    let mut seed = [0u8; 32];
    seed.copy_from_slice(&bytes);
    Ok(seed)
}

pub fn verifying_key(pubkey_hex: &str) -> Result<VerifyingKey, String> {
    let bytes = from_hex(pubkey_hex).map_err(|e| format!("public key: {e}"))?;
    let arr: [u8; 32] = bytes
        .try_into()
        .map_err(|_| "public key is not 32 bytes".to_string())?;
    VerifyingKey::from_bytes(&arr).map_err(|e| format!("public key: {e}"))
}

/// Derive the public key from a seed file (the operator's side of keygen).
pub fn pubkey_from_seed_file(path: &Path) -> Result<String, String> {
    let seed = load_seed_hex(path)?;
    Ok(to_hex(
        SigningKey::from_bytes(&seed).verifying_key().as_bytes(),
    ))
}

/// Detached signature over the exact manifest bytes, hex encoded on disk.
pub fn sign(manifest_bytes: &[u8], seed_hex: &str) -> Result<String, String> {
    let bytes = from_hex(seed_hex).map_err(|e| format!("seed: {e}"))?;
    let seed: [u8; 32] = bytes
        .try_into()
        .map_err(|_| "seed is not 32 bytes".to_string())?;
    let key = SigningKey::from_bytes(&seed);
    Ok(to_hex(&key.sign(manifest_bytes).to_bytes()))
}

pub fn verify(manifest_bytes: &[u8], sig_hex: &str, pubkey_hex: &str) -> Result<(), String> {
    let key = verifying_key(pubkey_hex)?;
    let sig_bytes = from_hex(sig_hex).map_err(|e| format!("signature: {e}"))?;
    let sig_arr: [u8; 64] = sig_bytes
        .try_into()
        .map_err(|_| format!("signature is not 64 bytes ({} given)", sig_hex.len() / 2))?;
    let sig = Signature::from_bytes(&sig_arr);
    key.verify(manifest_bytes, &sig).map_err(|_| {
        "manifest signature does not verify against the built-in public key".to_string()
    })
}

/// Generate a key on the dev machine. The seed file is created with mode 0600
/// and is never written into the repository.
pub fn keygen(path: &Path) -> Result<(String, String), String> {
    if path.exists() {
        return Err(format!(
            "{} already exists — refusing to overwrite a signing key",
            path.display()
        ));
    }
    let mut seed = [0u8; 32];
    let mut f = std::fs::File::open("/dev/urandom").map_err(|e| format!("urandom: {e}"))?;
    f.read_exact(&mut seed)
        .map_err(|e| format!("urandom: {e}"))?;
    let key = SigningKey::from_bytes(&seed);
    let pub_hex = to_hex(key.verifying_key().as_bytes());
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .map_err(|e| format!("create {}: {e}", path.display()))?;
        writeln!(file, "{}", to_hex(&seed)).map_err(|e| e.to_string())?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, format!("{}\n", to_hex(&seed))).map_err(|e| e.to_string())?;
    }
    Ok((pub_hex.clone(), fingerprint(&pub_hex)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_and_verify_round_trip_and_reject_tampering() {
        let (pub_hex, fp) = {
            let seed = [7u8; 32];
            let key = SigningKey::from_bytes(&seed);
            let pub_hex = to_hex(key.verifying_key().as_bytes());
            let fp = fingerprint(&pub_hex);
            (pub_hex, fp)
        };
        assert_eq!(fp.len(), 16);
        let seed_hex = to_hex(&[7u8; 32]);
        let msg = b"hello manifest";
        let sig = sign(msg, &seed_hex).unwrap();
        verify(msg, &sig, &pub_hex).unwrap();
        // One flipped byte in the message fails.
        verify(b"hello manifesT", &sig, &pub_hex).unwrap_err();
        // A truncated signature is a named error, not a panic.
        assert!(verify(msg, "00", &pub_hex)
            .unwrap_err()
            .contains("64 bytes"));
        // Malformed key hex is named too.
        assert!(verify(msg, &sig, "zz").unwrap_err().contains("public key"));
    }

    #[test]
    fn sha256_matches_known_vector() {
        assert_eq!(
            sha256_bytes(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn keygen_refuses_to_overwrite() {
        let dir = std::env::temp_dir().join(format!("hm-release-keygen-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("k.hex");
        let (pub_hex, _) = keygen(&path).unwrap();
        assert_eq!(pub_hex.len(), 64);
        keygen(&path).unwrap_err();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
