#![expect(clippy::panic_in_result_fn, reason = "assertions in tests")]

use super::*;
use crate::{ColumnFamilyDefinition, Db, DbOptions};
use std::error::Error;
use tempfile::TempDir;

type TestResult = Result<(), Box<dyn Error>>;

const KEY_A: [u8; 32] = [0x11; 32];
const KEY_B: [u8; 32] = [0x22; 32];
const NONCE_LEN: usize = 24;
const TAG_LEN: usize = 16;

fn key(bytes: [u8; 32]) -> VaultKey {
    VaultKey::new(bytes)
}

fn is_corruption<T>(result: &Result<T, KvError>) -> bool {
    matches!(result, Err(KvError::Corruption(_)))
}

// --- VaultKey ---

#[test]
fn vault_key_accepts_exactly_32_bytes() -> TestResult {
    let k = VaultKey::try_from(&KEY_A[..])?;
    assert_eq!(k.as_bytes(), &KEY_A, "key bytes are kept as given");
    for len in [0, 1, 16, 31, 33, 64] {
        let bytes = vec![7_u8; len];
        assert!(
            VaultKey::try_from(bytes.as_slice()).is_err(),
            "a {len}-byte key must be rejected"
        );
    }
    Ok(())
}

#[test]
fn vault_key_debug_does_not_leak_key_material() {
    let k = key([0xAB; 32]);
    let rendered = format!("{k:?}");
    for needle in ["171", "ab", "AB", "0xab", "0xAB"] {
        assert!(
            !rendered.contains(needle),
            "Debug output {rendered:?} leaks key material ({needle})"
        );
    }
    let options = VaultOptions {
        key: k,
        vault_column_families: vec!["a"],
    };
    let rendered = format!("{options:?}");
    assert!(
        !rendered.contains("171"),
        "VaultOptions Debug leaks the key: {rendered}"
    );
}

#[test]
fn vault_key_is_zeroized() {
    fn assert_zeroize_on_drop<T: ZeroizeOnDrop>() {}
    assert_zeroize_on_drop::<VaultKey>();
    let mut k = key(KEY_A);
    k.zeroize();
    assert_eq!(k.as_bytes(), &[0; 32], "zeroize must clear the key bytes");
}

// --- seal / open ---

#[test]
fn seal_open_round_trip() -> TestResult {
    let k = key(KEY_A);
    for plaintext in [&b""[..], b"x", b"hello vault", &[0_u8; 4096][..]] {
        let sealed = seal(&k, "cf", b"key", plaintext)?;
        assert_eq!(open(&k, "cf", b"key", &sealed)?, plaintext);
    }
    Ok(())
}

#[test]
fn seal_round_trips_empty_record_key_and_binary_data() -> TestResult {
    let k = key(KEY_A);
    let plaintext: Vec<u8> = (0..=255).collect();
    let sealed = seal(&k, "cf", b"", &plaintext)?;
    assert_eq!(open(&k, "cf", b"", &sealed)?, plaintext);
    Ok(())
}

#[test]
fn sealed_value_hides_plaintext_and_has_fixed_overhead() -> TestResult {
    let k = key(KEY_A);
    let plaintext = b"a very recognisable plaintext value";
    let sealed = seal(&k, "cf", b"key", plaintext)?;
    const {
        assert!(
            SEAL_OVERHEAD >= NONCE_LEN + TAG_LEN,
            "overhead must hold at least the nonce and the tag"
        );
    }
    assert_eq!(sealed.len(), plaintext.len() + SEAL_OVERHEAD);
    assert!(
        !sealed.windows(plaintext.len()).any(|w| w == plaintext),
        "the plaintext must not appear in the sealed value"
    );
    assert_eq!(seal(&k, "cf", b"key", b"")?.len(), SEAL_OVERHEAD);
    Ok(())
}

#[test]
fn nonce_is_random_per_value() -> TestResult {
    let k = key(KEY_A);
    let a = seal(&k, "cf", b"key", b"same")?;
    let b = seal(&k, "cf", b"key", b"same")?;
    assert_ne!(a, b, "sealing twice must give different outputs");
    // The 24-byte nonce prefix differs too (after any header, so compare
    // whole non-tag bodies is not possible; compare the nonce region)
    let mut seen = std::collections::HashSet::new();
    for _ in 0..64 {
        let sealed = seal(&k, "cf", b"key", b"same")?;
        assert!(seen.insert(sealed), "sealed values must never repeat");
    }
    Ok(())
}

#[test]
fn wrong_key_is_corruption() -> TestResult {
    let sealed = seal(&key(KEY_A), "cf", b"key", b"secret")?;
    assert!(is_corruption(&open(&key(KEY_B), "cf", b"key", &sealed)));
    Ok(())
}

#[test]
fn any_flipped_bit_is_corruption() -> TestResult {
    let k = key(KEY_A);
    let sealed = seal(&k, "cf", b"key", b"secret value")?;
    for index in 0..sealed.len() {
        for bit in [0x01, 0x80] {
            let mut tampered = sealed.clone();
            tampered[index] ^= bit;
            assert!(
                is_corruption(&open(&k, "cf", b"key", &tampered)),
                "flipping bit {bit:#x} of byte {index} must be detected"
            );
        }
    }
    Ok(())
}

#[test]
fn truncation_is_corruption() -> TestResult {
    let k = key(KEY_A);
    let sealed = seal(&k, "cf", b"key", b"secret value")?;
    for len in 0..sealed.len() {
        assert!(
            is_corruption(&open(&k, "cf", b"key", &sealed[..len])),
            "a value truncated to {len} bytes must be rejected"
        );
    }
    Ok(())
}

#[test]
fn extension_is_corruption() -> TestResult {
    let k = key(KEY_A);
    let mut sealed = seal(&k, "cf", b"key", b"secret value")?;
    sealed.push(0);
    assert!(is_corruption(&open(&k, "cf", b"key", &sealed)));
    Ok(())
}

#[test]
fn input_shorter_than_overhead_is_corruption_not_panic() {
    let k = key(KEY_A);
    for len in 0..=SEAL_OVERHEAD.max(1) {
        let junk = vec![0_u8; len];
        assert!(
            is_corruption(&open(&k, "cf", b"key", &junk)),
            "{len} zero bytes are not a sealed value"
        );
    }
}

#[test]
fn value_swapped_to_another_record_key_is_corruption() -> TestResult {
    let k = key(KEY_A);
    let sealed = seal(&k, "cf", b"key-1", b"secret")?;
    assert!(is_corruption(&open(&k, "cf", b"key-2", &sealed)));
    assert!(is_corruption(&open(&k, "cf", b"key-", &sealed)));
    assert!(is_corruption(&open(&k, "cf", b"key-11", &sealed)));
    assert!(is_corruption(&open(&k, "cf", b"", &sealed)));
    Ok(())
}

#[test]
fn value_swapped_to_another_column_family_is_corruption() -> TestResult {
    let k = key(KEY_A);
    let sealed = seal(&k, "cf1", b"key", b"secret")?;
    assert!(is_corruption(&open(&k, "cf2", b"key", &sealed)));
    assert!(is_corruption(&open(&k, "cf", b"key", &sealed)));
    assert!(is_corruption(&open(&k, "", b"key", &sealed)));
    Ok(())
}

#[test]
fn associated_data_is_unambiguous() -> TestResult {
    // cf="ab", key="c" must not equal cf="a", key="bc"
    let k = key(KEY_A);
    let sealed = seal(&k, "ab", b"c", b"secret")?;
    assert!(is_corruption(&open(&k, "a", b"bc", &sealed)));
    let sealed = seal(&k, "a", b"bc", b"secret")?;
    assert!(is_corruption(&open(&k, "ab", b"c", &sealed)));
    Ok(())
}

#[test]
fn two_values_of_the_same_record_are_not_interchangeable_across_keys() -> TestResult {
    let k = key(KEY_A);
    let one = seal(&k, "cf", b"k1", b"one")?;
    let two = seal(&k, "cf", b"k2", b"two")?;
    assert_eq!(open(&k, "cf", b"k1", &one)?, b"one");
    assert_eq!(open(&k, "cf", b"k2", &two)?, b"two");
    assert!(is_corruption(&open(&k, "cf", b"k1", &two)));
    assert!(is_corruption(&open(&k, "cf", b"k2", &one)));
    Ok(())
}

// --- options wiring and non-regression of plain databases ---

#[test]
fn vault_options_are_wired_into_db_options() {
    assert!(DbOptions::default().vault.is_none(), "plain by default");
    let options = DbOptions {
        vault: Some(VaultOptions {
            key: key(KEY_A),
            vault_column_families: vec!["a"],
        }),
        ..DbOptions::default()
    };
    let cloned = options.clone();
    let vault = cloned.vault.as_ref();
    assert_eq!(
        vault.map(|v| v.vault_column_families.clone()),
        Some(vec!["a"])
    );
    assert_eq!(vault.map(|v| *v.key.as_bytes()), Some(KEY_A));
}

#[test]
fn plain_db_is_unchanged_when_the_feature_is_compiled_in() -> TestResult {
    let dir = TempDir::new()?;
    let defs = vec![ColumnFamilyDefinition {
        name: "a",
        use_iter: true,
        min_prefix_size: 0,
        unordered_writes: false,
    }];
    let db = Db::open_read_write(dir.path(), defs, DbOptions::default())?;
    let cf = db.column_family("a")?;
    db.insert(&cf, b"k", b"plain value")?;
    assert_eq!(db.get(&cf, b"k")?.as_deref(), Some(&b"plain value"[..]));
    drop(db);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("OXNATIVE"))?,
        "oxnative 1\nengine fjall 3\n",
        "the marker of a plain database is unchanged"
    );
    Ok(())
}
