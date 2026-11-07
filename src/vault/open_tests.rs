//! Fail-closed open, key check and format marker (#5).

#![expect(clippy::panic_in_result_fn, reason = "assertions in tests")]

use super::*;
use crate::{ColumnFamilyDefinition, Db, DbOptions, META_KEYSPACE};
use fjall::KeyspaceCreateOptions;
use std::error::Error;
use std::fs;
use std::path::Path;
use tempfile::TempDir;

type TestResult = Result<(), Box<dyn Error>>;

const KEY_A: [u8; 32] = [0x11; 32];
const KEY_B: [u8; 32] = [0x22; 32];
const MARKER_PATH: &str = "OXNATIVE";
const PLAIN_MARKER: &str = "oxnative 1\nengine fjall 3\n";
const VAULT_MARKER: &str = "oxnative 1\nengine fjall 3\nvault 1\n";

fn definitions() -> Vec<ColumnFamilyDefinition> {
    ["plain", "secret"]
        .into_iter()
        .map(|name| ColumnFamilyDefinition {
            name,
            use_iter: true,
            min_prefix_size: 0,
            unordered_writes: false,
        })
        .collect()
}

fn vault_options(key: [u8; 32]) -> VaultOptions {
    VaultOptions {
        key: VaultKey::new(key),
        vault_column_families: vec!["secret"],
    }
}

fn open_vault(path: &Path, key: [u8; 32]) -> Result<Db, KvError> {
    Db::open_read_write(
        path,
        definitions(),
        DbOptions {
            vault: Some(vault_options(key)),
            ..DbOptions::default()
        },
    )
}

fn open_plain(path: &Path) -> Result<Db, KvError> {
    Db::open_read_write(path, definitions(), DbOptions::default())
}

fn create_vault_db(path: &Path, key: [u8; 32]) -> TestResult {
    let db = open_vault(path, key)?;
    let plain = db.column_family("plain")?;
    let mut transaction = db.start_transaction()?;
    transaction.insert(&plain, b"k", b"plain value");
    transaction.commit()?;
    db.flush()?;
    Ok(())
}

/// Entries of the metadata keyspace, read with the database closed.
fn meta_entries(path: &Path) -> Result<Vec<(Vec<u8>, Vec<u8>)>, Box<dyn Error>> {
    let db = fjall::Database::builder(path.join("fjall")).open()?;
    let meta = db.keyspace(META_KEYSPACE, KeyspaceCreateOptions::default)?;
    let mut entries = Vec::new();
    for guard in meta.iter() {
        let (key, value) = guard.into_inner()?;
        entries.push((key.to_vec(), value.to_vec()));
    }
    Ok(entries)
}

fn rewrite_meta(
    path: &Path,
    f: impl Fn(&fjall::Keyspace, &[u8], &[u8]) -> Result<(), Box<dyn Error>>,
) -> TestResult {
    let entries = meta_entries(path)?;
    let db = fjall::Database::builder(path.join("fjall")).open()?;
    let meta = db.keyspace(META_KEYSPACE, KeyspaceCreateOptions::default)?;
    for (key, value) in &entries {
        f(&meta, key, value)?;
    }
    db.persist(fjall::PersistMode::SyncAll)?;
    Ok(())
}

fn files_contain(dir: &Path, needle: &[u8]) -> Result<bool, Box<dyn Error>> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            if files_contain(&path, needle)? {
                return Ok(true);
            }
        } else if fs::read(&path)?
            .windows(needle.len())
            .any(|window| window == needle)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

#[test]
fn new_vault_database_marker_carries_the_vault_flag() -> TestResult {
    let dir = TempDir::new()?;
    create_vault_db(dir.path(), KEY_A)?;
    assert_eq!(
        fs::read_to_string(dir.path().join(MARKER_PATH))?,
        VAULT_MARKER,
        "the marker keeps its first lines and adds a `vault 1` line"
    );
    Ok(())
}

#[test]
fn new_vault_database_stores_a_key_check_but_never_the_key() -> TestResult {
    let dir = TempDir::new()?;
    create_vault_db(dir.path(), KEY_A)?;
    assert!(
        !meta_entries(dir.path())?.is_empty(),
        "a key-check value must be stored in {META_KEYSPACE}"
    );
    assert!(
        !files_contain(dir.path(), &KEY_A)?,
        "the raw key must never reach the disk"
    );
    Ok(())
}

#[test]
fn key_check_is_not_a_constant_across_databases() -> TestResult {
    let first = TempDir::new()?;
    let second = TempDir::new()?;
    create_vault_db(first.path(), KEY_A)?;
    create_vault_db(second.path(), KEY_A)?;
    let first_meta = meta_entries(first.path())?;
    assert!(!first_meta.is_empty(), "key-check value must exist");
    assert_ne!(
        first_meta,
        meta_entries(second.path())?,
        "the same key must not yield a predictable stored check"
    );
    Ok(())
}

#[test]
fn reopen_with_the_right_key_works_and_keeps_plain_data() -> TestResult {
    let dir = TempDir::new()?;
    create_vault_db(dir.path(), KEY_A)?;
    let db = open_vault(dir.path(), KEY_A)?;
    let plain = db.column_family("plain")?;
    assert!(db.contains_key(&plain, b"k")?);
    drop(db);
    assert_eq!(
        fs::read_to_string(dir.path().join(MARKER_PATH))?,
        VAULT_MARKER,
        "reopening must not rewrite the marker"
    );
    Ok(())
}

#[test]
fn reopen_with_a_wrong_key_fails_without_leaking_it() -> TestResult {
    let dir = TempDir::new()?;
    create_vault_db(dir.path(), KEY_A)?;
    let Err(error) = open_vault(dir.path(), KEY_B) else {
        return Err("opening with the wrong key must fail".into());
    };
    let message = error.to_string();
    assert!(
        message.to_lowercase().contains("vault") || message.to_lowercase().contains("key"),
        "the error must say that the key is the problem: {message}"
    );
    assert!(!message.contains("0x22") && !message.contains("34, 34"));
    Ok(())
}

#[test]
fn failed_open_does_not_modify_the_database() -> TestResult {
    let dir = TempDir::new()?;
    create_vault_db(dir.path(), KEY_A)?;
    let before = meta_entries(dir.path())?;
    assert!(!before.is_empty(), "key-check value must exist");
    assert!(open_vault(dir.path(), KEY_B).is_err());
    assert!(open_plain(dir.path()).is_err());
    assert_eq!(before, meta_entries(dir.path())?, "meta must be untouched");
    assert_eq!(
        fs::read_to_string(dir.path().join(MARKER_PATH))?,
        VAULT_MARKER
    );
    open_vault(dir.path(), KEY_A)?;
    Ok(())
}

#[test]
fn vault_database_opened_without_a_key_fails_closed() -> TestResult {
    let dir = TempDir::new()?;
    create_vault_db(dir.path(), KEY_A)?;
    assert!(
        open_plain(dir.path()).is_err(),
        "a missing key must be a hard error, never an empty or ciphertext view"
    );
    Ok(())
}

#[test]
fn read_only_open_requires_the_key() -> TestResult {
    let dir = TempDir::new()?;
    create_vault_db(dir.path(), KEY_A)?;
    assert!(
        Db::open_read_only(dir.path(), definitions()).is_err(),
        "read-only open without a key must fail"
    );
    assert!(
        Db::open_read_only_with_vault(dir.path(), definitions(), vault_options(KEY_B)).is_err(),
        "read-only open with a wrong key must fail"
    );
    let db = Db::open_read_only_with_vault(dir.path(), definitions(), vault_options(KEY_A))?;
    assert!(!db.is_writable());
    let plain = db.column_family("plain")?;
    assert!(db.contains_key(&plain, b"k")?);
    Ok(())
}

#[test]
fn read_only_vault_open_of_a_plain_database_fails() -> TestResult {
    let dir = TempDir::new()?;
    drop(open_plain(dir.path())?);
    assert!(
        Db::open_read_only_with_vault(dir.path(), definitions(), vault_options(KEY_A)).is_err()
    );
    Ok(())
}

#[test]
fn existing_plain_database_is_never_silently_vaulted() -> TestResult {
    let dir = TempDir::new()?;
    {
        let db = open_plain(dir.path())?;
        let secret = db.column_family("secret")?;
        let mut transaction = db.start_transaction()?;
        transaction.insert(&secret, b"k", b"already plaintext");
        transaction.commit()?;
    }
    assert!(
        open_vault(dir.path(), KEY_A).is_err(),
        "turning an existing plain database into a vault one would leave plaintext behind"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join(MARKER_PATH))?,
        PLAIN_MARKER,
        "the marker of a plain database must not change"
    );
    assert!(
        open_plain(dir.path()).is_ok(),
        "and it must keep opening plain"
    );
    Ok(())
}

#[test]
fn plain_database_with_the_feature_compiled_in_is_unchanged() -> TestResult {
    let dir = TempDir::new()?;
    drop(open_plain(dir.path())?);
    assert_eq!(
        fs::read_to_string(dir.path().join(MARKER_PATH))?,
        PLAIN_MARKER
    );
    assert!(
        meta_entries(dir.path())?.is_empty(),
        "no key check in plain DBs"
    );
    Ok(())
}

#[test]
fn stripping_the_vault_flag_from_the_marker_does_not_downgrade() -> TestResult {
    let dir = TempDir::new()?;
    create_vault_db(dir.path(), KEY_A)?;
    fs::write(dir.path().join(MARKER_PATH), PLAIN_MARKER)?;
    assert!(
        open_plain(dir.path()).is_err(),
        "key check present: a plain open must fail even without the marker flag"
    );
    assert!(
        open_vault(dir.path(), KEY_A).is_err(),
        "a marker that disagrees with the key check is corruption"
    );
    Ok(())
}

#[test]
fn adding_the_vault_flag_to_a_plain_marker_is_refused() -> TestResult {
    let dir = TempDir::new()?;
    {
        // An empty database is indistinguishable from an interrupted creation.
        let db = open_plain(dir.path())?;
        let plain = db.column_family("plain")?;
        let mut transaction = db.start_transaction()?;
        transaction.insert(&plain, b"k", b"plain value");
        transaction.commit()?;
    }
    fs::write(dir.path().join(MARKER_PATH), VAULT_MARKER)?;
    assert!(
        open_vault(dir.path(), KEY_A).is_err(),
        "no key check was ever stored: the database cannot be trusted as vaulted"
    );
    Ok(())
}

#[test]
fn tampered_truncated_or_removed_key_check_fails_closed() -> TestResult {
    type Mutation = fn(&fjall::Keyspace, &[u8], &[u8]) -> Result<(), Box<dyn Error>>;
    let mutations: [(&str, Mutation); 4] = [
        ("flipped byte", |meta, key, value| {
            let mut value = value.to_vec();
            if let Some(last) = value.last_mut() {
                *last ^= 0x01;
            }
            Ok(meta.insert(key, value)?)
        }),
        ("truncated", |meta, key, value| {
            Ok(meta.insert(key, &value[..value.len() / 2])?)
        }),
        ("emptied", |meta, key, _| Ok(meta.insert(key, b"")?)),
        ("removed", |meta, key, _| Ok(meta.remove(key)?)),
    ];
    for (name, mutation) in mutations {
        let dir = TempDir::new()?;
        create_vault_db(dir.path(), KEY_A)?;
        assert!(
            !meta_entries(dir.path())?.is_empty(),
            "key-check value must exist ({name})"
        );
        rewrite_meta(dir.path(), mutation)?;
        assert!(
            open_vault(dir.path(), KEY_A).is_err(),
            "a {name} key check must fail closed, not be recreated"
        );
    }
    Ok(())
}

#[test]
fn key_check_copied_from_another_database_is_rejected() -> TestResult {
    let victim = TempDir::new()?;
    let other = TempDir::new()?;
    create_vault_db(victim.path(), KEY_A)?;
    create_vault_db(other.path(), KEY_B)?;
    let foreign = meta_entries(other.path())?;
    assert!(!foreign.is_empty(), "key-check value must exist");
    rewrite_meta(victim.path(), |meta, key, _| Ok(meta.remove(key)?))?;
    {
        let db = fjall::Database::builder(victim.path().join("fjall")).open()?;
        let meta = db.keyspace(META_KEYSPACE, KeyspaceCreateOptions::default)?;
        for (key, value) in &foreign {
            meta.insert(key.as_slice(), value.as_slice())?;
        }
        db.persist(fjall::PersistMode::SyncAll)?;
    }
    assert!(
        open_vault(victim.path(), KEY_A).is_err(),
        "a check sealed under another key must not validate"
    );
    Ok(())
}

#[test]
fn vault_marker_without_key_check_in_an_empty_database_is_completed() -> TestResult {
    // A crash after the marker was written but before the key check.
    let dir = TempDir::new()?;
    fs::write(dir.path().join(MARKER_PATH), VAULT_MARKER)?;
    drop(open_vault(dir.path(), KEY_A)?);
    assert!(
        open_vault(dir.path(), KEY_B).is_err(),
        "the check was written"
    );
    drop(open_vault(dir.path(), KEY_A)?);
    Ok(())
}

#[test]
fn missing_key_check_with_data_still_fails_closed() -> TestResult {
    let dir = TempDir::new()?;
    create_vault_db(dir.path(), KEY_A)?;
    rewrite_meta(dir.path(), |meta, key, _| {
        meta.remove(key)?;
        Ok(())
    })?;
    assert!(open_vault(dir.path(), KEY_B).is_err());
    Ok(())
}

// --- the vault column family set is persisted and checked (#3 final review) ---

fn open_vault_with_set(
    path: &Path,
    key: [u8; 32],
    vault_column_families: Vec<&'static str>,
) -> Result<Db, KvError> {
    Db::open_read_write(
        path,
        definitions(),
        DbOptions {
            vault: Some(VaultOptions {
                key: VaultKey::new(key),
                vault_column_families,
            }),
            ..DbOptions::default()
        },
    )
}

#[test]
fn reopening_with_an_omitted_vault_cf_is_refused() -> TestResult {
    let dir = TempDir::new()?;
    {
        let db = open_vault(dir.path(), KEY_A)?;
        db.insert(&db.column_family("secret")?, b"k", b"v")?;
        db.flush()?;
    }
    assert!(open_vault_with_set(dir.path(), KEY_A, vec![]).is_err());
    assert!(open_vault_with_set(dir.path(), KEY_A, vec!["secret", "plain"]).is_err());
    let read_only = Db::open_read_only_with_vault(
        dir.path(),
        definitions(),
        VaultOptions {
            key: VaultKey::new(KEY_A),
            vault_column_families: vec![],
        },
    );
    assert!(read_only.is_err());
    drop(open_vault(dir.path(), KEY_A)?);
    Ok(())
}

#[test]
fn vault_cf_order_and_duplicates_do_not_matter() -> TestResult {
    let dir = TempDir::new()?;
    drop(open_vault_with_set(
        dir.path(),
        KEY_A,
        vec!["secret", "plain"],
    )?);
    drop(open_vault_with_set(
        dir.path(),
        KEY_A,
        vec!["plain", "secret", "plain"],
    )?);
    Ok(())
}

#[test]
fn backup_keeps_the_vault_cf_set() -> TestResult {
    let dir = TempDir::new()?;
    let target = dir.path().join("backup");
    open_vault(&dir.path().join("db"), KEY_A)?.backup(&target)?;
    assert!(open_vault_with_set(&target, KEY_A, vec![]).is_err());
    drop(open_vault(&target, KEY_A)?);
    Ok(())
}

// --- vaulted databases are refused by pre-vault builds (#11) ---

/// The marker check of the build before the vault feature, copied verbatim
/// from `main:src/lib.rs` (`check_marker`): it compares the first line only.
fn old_build_accepts_marker(path: &Path) -> Result<bool, Box<dyn Error>> {
    const OLD_MARKER_CONTENT: &str = "oxnative 1\nengine fjall 3\n";
    let content = fs::read_to_string(path.join(MARKER_PATH))?;
    Ok(content.lines().next() == OLD_MARKER_CONTENT.lines().next())
}

#[test]
fn vault_and_plain_markers_have_different_first_lines() -> TestResult {
    let plain_dir = TempDir::new()?;
    drop(open_plain(plain_dir.path())?);
    let vault_dir = TempDir::new()?;
    create_vault_db(vault_dir.path(), KEY_A)?;
    let plain = fs::read_to_string(plain_dir.path().join(MARKER_PATH))?;
    let vault = fs::read_to_string(vault_dir.path().join(MARKER_PATH))?;
    assert_eq!(plain.lines().next(), Some("oxnative 1"));
    assert_eq!(
        vault.lines().next(),
        Some("oxnative 2"),
        "a vaulted database must change the first marker line"
    );
    assert_eq!(
        vault, "oxnative 2\nengine fjall 3\nvault 1\n",
        "the rest of the vault marker is unchanged"
    );
    Ok(())
}

#[test]
fn first_line_only_check_of_the_old_build_rejects_a_vaulted_database() -> TestResult {
    let vault_dir = TempDir::new()?;
    create_vault_db(vault_dir.path(), KEY_A)?;
    assert!(
        !old_build_accepts_marker(vault_dir.path())?,
        "a pre-vault build would open the database and write plaintext into vault CFs"
    );
    let plain_dir = TempDir::new()?;
    drop(open_plain(plain_dir.path())?);
    assert!(
        old_build_accepts_marker(plain_dir.path())?,
        "sanity: the replicated old check accepts a plain database"
    );
    Ok(())
}

#[test]
fn vault_marker_stays_after_reopen_and_still_rejected_by_old_check() -> TestResult {
    let dir = TempDir::new()?;
    create_vault_db(dir.path(), KEY_A)?;
    drop(open_vault(dir.path(), KEY_A)?);
    assert!(!old_build_accepts_marker(dir.path())?);
    Ok(())
}
