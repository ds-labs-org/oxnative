//! Encrypted-at-rest column families (feature `vault`).

use crate::KvError;
use std::fmt;
use zeroize::{Zeroize, ZeroizeOnDrop};

/// Bytes added to a plaintext value by [`seal`].
pub(crate) const SEAL_OVERHEAD: usize = 0;

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

// Red step stubs: the real impls zeroize the bytes
impl Zeroize for VaultKey {
    fn zeroize(&mut self) {}
}

impl ZeroizeOnDrop for VaultKey {}

impl TryFrom<&[u8]> for VaultKey {
    type Error = KvError;

    #[expect(clippy::unimplemented, reason = "red step stub")]
    fn try_from(_bytes: &[u8]) -> Result<Self, KvError> {
        unimplemented!("VaultKey::try_from")
    }
}

impl fmt::Debug for VaultKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "VaultKey({:?})", self.0)
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

#[expect(clippy::unimplemented, reason = "red step stub")]
pub(crate) fn seal(
    _key: &VaultKey,
    _column_family: &str,
    _record_key: &[u8],
    _plaintext: &[u8],
) -> Result<Vec<u8>, KvError> {
    unimplemented!("seal")
}

#[expect(clippy::unimplemented, reason = "red step stub")]
pub(crate) fn open(
    _key: &VaultKey,
    _column_family: &str,
    _record_key: &[u8],
    _sealed: &[u8],
) -> Result<Vec<u8>, KvError> {
    unimplemented!("open")
}

#[cfg(test)]
mod tests;
