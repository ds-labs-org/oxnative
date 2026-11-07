//! Encrypted-at-rest column families (feature `vault`).

use crate::{KvError, META_KEYSPACE, VAULT_CHECK_KEY};
use chacha20poly1305::aead::{Aead, KeyInit, OsRng, Payload};
use chacha20poly1305::{AeadCore, Key, XChaCha20Poly1305, XNonce};
use std::fmt;
use std::sync::Arc;
use zeroize::{Zeroize, ZeroizeOnDrop};

const FORMAT_VERSION: u8 = 1;
/// Identifies the key that sealed a value. Always 0 for now, reserved so that
/// key rotation can be added without changing the sealed format.
const KEY_ID: u8 = 0;
const HEADER_LEN: usize = 2;
const NONCE_LEN: usize = 24;
const TAG_LEN: usize = 16;

/// Bytes added to a plaintext value by [`seal`]: header (format version and
/// key id), nonce and authentication tag.
pub(crate) const SEAL_OVERHEAD: usize = HEADER_LEN + NONCE_LEN + TAG_LEN;

/// A 32-byte encryption key, zeroized on drop.
#[derive(Clone)]
pub struct VaultKey([u8; 32]);

impl VaultKey {
    /// Wraps raw key bytes.
    #[must_use]
    pub fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub(crate) fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl Zeroize for VaultKey {
    fn zeroize(&mut self) {
        self.0.zeroize();
    }
}

impl Drop for VaultKey {
    fn drop(&mut self) {
        self.zeroize();
    }
}

impl ZeroizeOnDrop for VaultKey {}

impl TryFrom<&[u8]> for VaultKey {
    type Error = KvError;

    fn try_from(bytes: &[u8]) -> Result<Self, KvError> {
        let key = <[u8; 32]>::try_from(bytes).map_err(|_| {
            KvError::other(format!(
                "a vault key must be exactly 32 bytes, got {}",
                bytes.len()
            ))
        })?;
        Ok(Self(key))
    }
}

impl fmt::Debug for VaultKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("VaultKey(<redacted>)")
    }
}

/// Which column families are vaulted, and the key protecting them.
#[derive(Clone, Debug)]
pub struct VaultOptions {
    /// Key encrypting the values of the vault column families.
    pub key: VaultKey,
    /// Names of the column families whose values are encrypted.
    pub vault_column_families: Vec<&'static str>,
}

/// Encrypts and decrypts the values of one vault column family.
#[derive(Clone)]
pub(crate) struct Seal {
    key: Arc<VaultKey>,
    column_family: &'static str,
}

impl Seal {
    pub(crate) fn new(key: Arc<VaultKey>, column_family: &'static str) -> Self {
        Self { key, column_family }
    }

    pub(crate) fn column_family(&self) -> &'static str {
        self.column_family
    }

    pub(crate) fn seal(&self, record_key: &[u8], plaintext: &[u8]) -> Result<Vec<u8>, KvError> {
        seal(&self.key, self.column_family, record_key, plaintext)
    }

    pub(crate) fn open(&self, record_key: &[u8], sealed: &[u8]) -> Result<Vec<u8>, KvError> {
        open(&self.key, self.column_family, record_key, sealed)
    }
}

const CHECK_PLAINTEXT: &[u8] = b"oxnative vault key check";

/// Canonical encoding of a set of vault column family names: sorted, without
/// duplicates, each name prefixed by its length.
pub(crate) fn encode_column_families(names: &[&str]) -> Result<Vec<u8>, KvError> {
    let mut sorted = names.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    let mut encoded = Vec::new();
    for name in sorted {
        let len = u8::try_from(name.len())
            .map_err(|_| KvError::other(format!("column family name {name:?} too long")))?;
        encoded.push(len);
        encoded.extend_from_slice(name.as_bytes());
    }
    Ok(encoded)
}

/// Location of the key check, which binds it to the vault column family set.
fn check_location(column_families: &[u8]) -> Vec<u8> {
    let mut location = VAULT_CHECK_KEY.to_vec();
    location.extend_from_slice(column_families);
    location
}

/// Builds the value stored to recognize the key at the next open: a sealed
/// constant, bound to its location and to the encoded vault column family set,
/// so it differs on every database.
pub(crate) fn new_check(key: &VaultKey, column_families: &[u8]) -> Result<Vec<u8>, KvError> {
    seal(
        key,
        META_KEYSPACE,
        &check_location(column_families),
        CHECK_PLAINTEXT,
    )
}

/// Checks `key` against a stored key check for the stored column family set.
pub(crate) fn verify_check(
    key: &VaultKey,
    column_families: &[u8],
    stored: &[u8],
) -> Result<(), KvError> {
    match open(key, META_KEYSPACE, &check_location(column_families), stored) {
        Ok(plaintext) if plaintext == CHECK_PLAINTEXT => Ok(()),
        _ => Err(KvError::other(
            "wrong vault key, or the vault key check is damaged",
        )),
    }
}

/// Associated data binding a value to its column family and record key. The
/// column family length prefix keeps the encoding unambiguous.
fn associated_data(
    header: [u8; HEADER_LEN],
    column_family: &str,
    record_key: &[u8],
) -> Result<Vec<u8>, KvError> {
    let cf_len = u64::try_from(column_family.len())
        .map_err(|_| KvError::other("column family name too long"))?;
    let mut aad = Vec::with_capacity(HEADER_LEN + 8 + column_family.len() + record_key.len());
    aad.extend_from_slice(&header);
    aad.extend_from_slice(&cf_len.to_le_bytes());
    aad.extend_from_slice(column_family.as_bytes());
    aad.extend_from_slice(record_key);
    Ok(aad)
}

/// Encrypts `plaintext` as `header || nonce || ciphertext || tag` with a fresh
/// random nonce.
pub(crate) fn seal(
    key: &VaultKey,
    column_family: &str,
    record_key: &[u8],
    plaintext: &[u8],
) -> Result<Vec<u8>, KvError> {
    let header = [FORMAT_VERSION, KEY_ID];
    let aad = associated_data(header, column_family, record_key)?;
    let cipher = XChaCha20Poly1305::new(Key::from_slice(key.as_bytes()));
    let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
    let ciphertext = cipher
        .encrypt(
            &nonce,
            Payload {
                msg: plaintext,
                aad: &aad,
            },
        )
        .map_err(|_| KvError::other("vault encryption failed"))?;
    let mut sealed = Vec::with_capacity(HEADER_LEN + NONCE_LEN + ciphertext.len());
    sealed.extend_from_slice(&header);
    sealed.extend_from_slice(&nonce);
    sealed.extend_from_slice(&ciphertext);
    Ok(sealed)
}

/// Decrypts a value produced by [`seal`]. Any failure (wrong key, tampering,
/// wrong location, malformed input) is reported as corruption.
pub(crate) fn open(
    key: &VaultKey,
    column_family: &str,
    record_key: &[u8],
    sealed: &[u8],
) -> Result<Vec<u8>, KvError> {
    let corrupt = || KvError::corruption("vault value cannot be decrypted");
    if sealed.len() < SEAL_OVERHEAD {
        return Err(corrupt());
    }
    let (header, rest) = sealed.split_at(HEADER_LEN);
    let header = <[u8; HEADER_LEN]>::try_from(header).map_err(|_| corrupt())?;
    if header != [FORMAT_VERSION, KEY_ID] {
        return Err(corrupt());
    }
    let (nonce, ciphertext) = rest.split_at(NONCE_LEN);
    let aad = associated_data(header, column_family, record_key)?;
    let cipher = XChaCha20Poly1305::new(Key::from_slice(key.as_bytes()));
    cipher
        .decrypt(
            XNonce::from_slice(nonce),
            Payload {
                msg: ciphertext,
                aad: &aad,
            },
        )
        .map_err(|_| corrupt())
}

#[cfg(test)]
mod backup_sst_tests;
#[cfg(test)]
mod fork_p0_tests;
#[cfg(test)]
mod open_tests;
#[cfg(test)]
mod rw_tests;
#[cfg(test)]
mod tests;
