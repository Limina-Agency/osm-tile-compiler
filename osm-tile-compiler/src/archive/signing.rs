//! Ed25519 keys and signatures, so a device can tell an update came from
//! someone it trusts before it applies it.
//!
//! A delta carries its signatures in a footer after its last operation
//! (`delta.rs`). Any other file, such as a whole archive, is signed with a
//! detached signature file beside it, `<FILE>.sig`, which is JSON:
//!
//! ```json
//! {"format": "osm-tile-compiler-signature", "version": 1,
//!  "bytes": 1234, "sha256": "…",
//!  "signatures": [{"algorithm": "ed25519", "publicKey": "…", "signature": "…"}]}
//! ```
//!
//! Each signature signs a message that starts with a label saying what is
//! signed, so a signature over a delta can never pass for one over an
//! archive: `osm-tile-compiler delta v2` or `osm-tile-compiler file v1`, a
//! zero byte, then the SHA-256 of what is signed (and for a file, its length
//! as a little-endian `u64`).
//!
//! A secret key file holds the key's 32-byte seed as 64 hex digits; lines
//! starting with `#` are comments. A public key is 64 hex digits too, given
//! as they are or in a file.

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

/// The detached signature file's `format`.
pub const SIGNATURE_FORMAT: &str = "osm-tile-compiler-signature";
/// The detached signature file's `version`.
pub const SIGNATURE_VERSION: u32 = 1;
/// What a delta signature signs, before the digest.
pub(super) const DELTA_LABEL: &[u8] = b"osm-tile-compiler delta v2\0";
const FILE_LABEL: &[u8] = b"osm-tile-compiler file v1\0";
/// A footer record's algorithm byte, and the JSON's `algorithm`.
pub(super) const ED25519: u8 = 1;
const ED25519_NAME: &str = "ed25519";

/// A key that checks signatures: one an app pins to trust updates.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct PublicKey(VerifyingKey);

impl PublicKey {
    /// 64 hex digits.
    pub fn from_hex(text: &str) -> Result<Self, String> {
        let bytes: [u8; 32] = hex::decode(text.trim())
            .ok()
            .and_then(|bytes| bytes.try_into().ok())
            .ok_or("a public key is 64 hex digits")?;
        Self::from_bytes(&bytes)
    }

    pub fn from_bytes(bytes: &[u8; 32]) -> Result<Self, String> {
        VerifyingKey::from_bytes(bytes)
            .map(Self)
            .map_err(|_| "not an ed25519 public key".to_string())
    }

    /// 64 hex digits given as they are, or the path of a file holding them,
    /// such as the `.pub` file [`SecretKey::write_pair`] writes.
    pub fn parse(text: &str) -> Result<Self, String> {
        if text.len() == 64 && text.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Self::from_hex(text);
        }
        let contents = std::fs::read_to_string(text)
            .map_err(|e| format!("{text} is neither a public key nor a readable file: {e}"))?;
        Self::from_hex(&uncommented(&contents)).map_err(|e| format!("{text}: {e}"))
    }

    pub fn to_bytes(&self) -> [u8; 32] {
        self.0.to_bytes()
    }

    pub fn to_hex(&self) -> String {
        hex::encode(self.0.to_bytes())
    }

    pub(super) fn verifies(&self, message: &[u8], signature: &[u8; 64]) -> bool {
        self.0
            .verify(message, &Signature::from_bytes(signature))
            .is_ok()
    }
}

impl fmt::Debug for PublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PublicKey({})", self.to_hex())
    }
}

impl fmt::Display for PublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

/// A key that signs deltas and files. Its `Debug` doesn't show it.
#[derive(Clone)]
pub struct SecretKey(SigningKey);

impl SecretKey {
    /// A new key from the operating system's random source.
    pub fn generate() -> Result<Self, String> {
        let mut seed = [0u8; 32];
        getrandom::fill(&mut seed).map_err(|e| format!("no random source: {e}"))?;
        Ok(Self(SigningKey::from_bytes(&seed)))
    }

    /// From its 32-byte seed.
    pub fn from_seed(seed: &[u8; 32]) -> Self {
        Self(SigningKey::from_bytes(seed))
    }

    /// Reads a secret key file: the seed as 64 hex digits.
    pub fn read(path: &Path) -> Result<Self, String> {
        let contents = std::fs::read_to_string(path)
            .map_err(|e| format!("Failed to read {}: {e}", path.display()))?;
        let seed: [u8; 32] = hex::decode(uncommented(&contents))
            .ok()
            .and_then(|bytes| bytes.try_into().ok())
            .ok_or_else(|| {
                format!(
                    "{} is not a secret key file: it holds 64 hex digits",
                    path.display()
                )
            })?;
        Ok(Self::from_seed(&seed))
    }

    pub fn public_key(&self) -> PublicKey {
        PublicKey(self.0.verifying_key())
    }

    /// Writes the secret key to `secret` (readable only by its owner on
    /// Unix) and its public key to `secret` with `.pub` appended. Neither may
    /// exist yet.
    pub fn write_pair(&self, secret: &Path) -> Result<PathBuf, String> {
        let public = with_suffix(secret, ".pub");
        for path in [secret, &public] {
            if path.exists() {
                return Err(format!("{} already exists", path.display()));
            }
        }
        let mut options = File::options();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut file = options
            .open(secret)
            .map_err(|e| format!("Failed to create {}: {e}", secret.display()))?;
        writeln!(
            file,
            "# osm-tile-compiler ed25519 secret key; keep it private\n{}",
            hex::encode(self.0.to_bytes())
        )
        .and_then(|()| file.sync_all())
        .map_err(|e| format!("Failed to write {}: {e}", secret.display()))?;
        std::fs::write(&public, format!("{}\n", self.public_key()))
            .map_err(|e| format!("Failed to write {}: {e}", public.display()))?;
        Ok(public)
    }

    pub(super) fn sign(&self, message: &[u8]) -> [u8; 64] {
        self.0.sign(message).to_bytes()
    }
}

impl fmt::Debug for SecretKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SecretKey(public {})", self.public_key())
    }
}

/// The text of a key file without its comment lines and spaces.
fn uncommented(contents: &str) -> String {
    contents
        .lines()
        .map(str::trim)
        .filter(|line| !line.starts_with('#'))
        .collect()
}

pub(super) fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// The message a delta signature signs.
pub(super) fn delta_message(sha256: &[u8; 32]) -> Vec<u8> {
    [DELTA_LABEL, sha256].concat()
}

fn file_message(sha256: &[u8; 32], bytes: u64) -> Vec<u8> {
    [FILE_LABEL, sha256, &bytes.to_le_bytes()].concat()
}

/// One signature, as a delta's footer or a signature file holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Record {
    pub key: [u8; 32],
    pub signature: [u8; 64],
}

/// What checking a file's or a delta's signatures found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct SignatureCheck {
    /// The public keys, in hex, whose signatures are valid, in the order
    /// they were added.
    pub signed_by: Vec<String>,
    /// Whether one of them is a key the caller trusts. Always false when the
    /// caller named no keys.
    pub trusted: bool,
}

/// Checks every signature in `records` over `message`: all must be valid,
/// and when `trusted` names keys, one must be by one of them.
pub(super) fn check(
    records: &[Record],
    message: &[u8],
    trusted: &[PublicKey],
    what: &str,
) -> Result<SignatureCheck, String> {
    let mut signed_by = Vec::with_capacity(records.len());
    let mut is_trusted = false;
    for record in records {
        let key = PublicKey::from_bytes(&record.key)
            .map_err(|_| format!("{what} is signed with a malformed key"))?;
        if !key.verifies(message, &record.signature) {
            return Err(format!(
                "{what} has a signature by {key} that does not match it: it was changed after it was signed"
            ));
        }
        is_trusted |= trusted.contains(&key);
        signed_by.push(key.to_hex());
    }
    if !trusted.is_empty() && !is_trusted {
        return Err(if signed_by.is_empty() {
            format!("{what} is not signed, and a signature by a trusted key is required")
        } else {
            format!(
                "{what} is signed only by keys that are not trusted: {}",
                signed_by.join(", ")
            )
        });
    }
    Ok(SignatureCheck {
        signed_by,
        trusted: is_trusted,
    })
}

/// Adds a signature by `key` to `records`, replacing an earlier one by the
/// same key.
pub(super) fn add(records: &mut Vec<Record>, key: &SecretKey, message: &[u8]) {
    let public = key.public_key().to_bytes();
    records.retain(|record| record.key != public);
    records.push(Record {
        key: public,
        signature: key.sign(message),
    });
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SignatureFile {
    format: String,
    version: u32,
    bytes: u64,
    sha256: String,
    signatures: Vec<SignatureEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SignatureEntry {
    algorithm: String,
    public_key: String,
    signature: String,
}

/// Where a file's detached signature is kept: `<FILE>.sig`.
pub fn signature_path(file: &Path) -> PathBuf {
    with_suffix(file, ".sig")
}

fn sha256_of(path: &Path) -> Result<([u8; 32], u64), String> {
    let mut file =
        File::open(path).map_err(|e| format!("Failed to open {}: {e}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1 << 20];
    let mut bytes = 0u64;
    loop {
        let read = std::io::Read::read(&mut file, &mut buffer)
            .map_err(|e| format!("Failed to read {}: {e}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        bytes += read as u64;
    }
    Ok((hasher.finalize().into(), bytes))
}

fn read_signature_file(path: &Path) -> Result<SignatureFile, String> {
    let text =
        std::fs::read(path).map_err(|e| format!("Failed to read {}: {e}", path.display()))?;
    let parsed: SignatureFile = serde_json::from_slice(&text)
        .map_err(|e| format!("{} is not a signature file: {e}", path.display()))?;
    if parsed.format != SIGNATURE_FORMAT || parsed.version != SIGNATURE_VERSION {
        return Err(format!(
            "{} is not a signature file this build reads",
            path.display()
        ));
    }
    Ok(parsed)
}

fn records_of(file: &SignatureFile, path: &Path) -> Result<Vec<Record>, String> {
    file.signatures
        .iter()
        .map(|entry| {
            let key = hex::decode(&entry.public_key)
                .ok()
                .and_then(|b| b.try_into().ok());
            let signature = hex::decode(&entry.signature)
                .ok()
                .and_then(|b| b.try_into().ok());
            match (entry.algorithm.as_str(), key, signature) {
                (ED25519_NAME, Some(key), Some(signature)) => Ok(Record { key, signature }),
                _ => Err(format!("{} holds a malformed signature", path.display())),
            }
        })
        .collect()
}

/// Signs `file` with `key` in its detached signature file, `<FILE>.sig`,
/// adding to the signatures already there when they are for the same bytes.
pub(super) fn sign_file(file: &Path, key: &SecretKey) -> Result<SignatureCheck, String> {
    let (sha256, bytes) = sha256_of(file)?;
    let path = signature_path(file);
    let mut records = Vec::new();
    if path.exists() {
        let existing = read_signature_file(&path)?;
        if existing.sha256 == hex::encode(sha256) && existing.bytes == bytes {
            records = records_of(&existing, &path)?;
        }
    }
    let message = file_message(&sha256, bytes);
    add(&mut records, key, &message);
    let checked = check(&records, &message, &[], &file.display().to_string())?;
    let contents = SignatureFile {
        format: SIGNATURE_FORMAT.into(),
        version: SIGNATURE_VERSION,
        bytes,
        sha256: hex::encode(sha256),
        signatures: records
            .iter()
            .map(|record| SignatureEntry {
                algorithm: ED25519_NAME.into(),
                public_key: hex::encode(record.key),
                signature: hex::encode(record.signature),
            })
            .collect(),
    };
    let mut json = serde_json::to_vec_pretty(&contents).map_err(|e| e.to_string())?;
    json.push(b'\n');
    let partial = with_suffix(&path, ".partial");
    std::fs::write(&partial, json)
        .and_then(|()| std::fs::rename(&partial, &path))
        .map_err(|e| {
            let _ = std::fs::remove_file(&partial);
            format!("Failed to write {}: {e}", path.display())
        })?;
    Ok(checked)
}

/// Checks `file` against its detached signature file `signature` (by
/// default `<FILE>.sig`): the bytes must be the ones signed, every signature
/// valid and, when `trusted` names keys, one by a trusted key.
pub(super) fn verify_file(
    file: &Path,
    signature: Option<&Path>,
    trusted: &[PublicKey],
) -> Result<SignatureCheck, String> {
    let path = signature.map_or_else(|| signature_path(file), Path::to_path_buf);
    let contents = read_signature_file(&path)?;
    let records = records_of(&contents, &path)?;
    let (sha256, bytes) = sha256_of(file)?;
    if contents.sha256 != hex::encode(sha256) || contents.bytes != bytes {
        return Err(format!(
            "{} is not the file {} signs: it was changed after it was signed",
            file.display(),
            path.display()
        ));
    }
    check(
        &records,
        &file_message(&sha256, bytes),
        trusted,
        &file.display().to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_round_trip_through_files() {
        let dir = tempfile::tempdir().unwrap();
        let secret = dir.path().join("release.key");
        let key = SecretKey::generate().unwrap();
        let public = key.write_pair(&secret).unwrap();
        assert_eq!(public, dir.path().join("release.key.pub"));
        let read = SecretKey::read(&secret).unwrap();
        assert_eq!(read.public_key(), key.public_key());
        let parsed = PublicKey::parse(public.to_str().unwrap()).unwrap();
        assert_eq!(parsed, key.public_key());
        assert_eq!(
            PublicKey::parse(&key.public_key().to_hex()).unwrap(),
            parsed
        );
        assert!(
            key.write_pair(&secret)
                .unwrap_err()
                .contains("already exists")
        );
        assert!(!format!("{read:?}").contains(&hex::encode(read.0.to_bytes())));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&secret).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0);
        }
    }

    #[test]
    fn a_file_signature_names_its_bytes_and_keys() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.pmtiles");
        std::fs::write(&file, b"archive bytes").unwrap();
        let (one, two) = (
            SecretKey::from_seed(&[1; 32]),
            SecretKey::from_seed(&[2; 32]),
        );
        sign_file(&file, &one).unwrap();
        let both = sign_file(&file, &two).unwrap();
        assert_eq!(
            both.signed_by,
            [one.public_key().to_hex(), two.public_key().to_hex()]
        );
        let checked = verify_file(&file, None, &[two.public_key()]).unwrap();
        assert!(checked.trusted);
        let other = SecretKey::from_seed(&[3; 32]).public_key();
        let refused = verify_file(&file, None, &[other]).unwrap_err();
        assert!(refused.contains("not trusted"), "{refused}");
        std::fs::write(&file, b"archive bytez").unwrap();
        let changed = verify_file(&file, None, &[]).unwrap_err();
        assert!(changed.contains("changed after it was signed"), "{changed}");
    }

    #[test]
    fn a_delta_signature_does_not_pass_for_a_file_signature() {
        let key = SecretKey::from_seed(&[1; 32]);
        let digest = [9u8; 32];
        let mut records = Vec::new();
        add(&mut records, &key, &delta_message(&digest));
        assert!(check(&records, &file_message(&digest, 0), &[], "x").is_err());
        assert!(check(&records, &delta_message(&digest), &[], "x").is_ok());
    }
}
