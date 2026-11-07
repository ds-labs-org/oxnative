//! Encrypted `backup()` and bulk-load files on vault column families (#7).

#![expect(clippy::panic_in_result_fn, reason = "assertions in tests")]

use super::*;
use crate::{ColumnFamily, ColumnFamilyDefinition, Db, DbOptions};
use fjall::KeyspaceCreateOptions;
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

type TestResult = Result<(), Box<dyn Error>>;

const KEY_A: [u8; 32] = [0x11; 32];
const KEY_B: [u8; 32] = [0x22; 32];
const PLAIN: &str = "plain";
const SECRET: &str = "secret";
const OTHER_SECRET: &str = "other_secret";
const SECRET_VALUE: &[u8] = b"distinctive-secret-value-0123456789";
const OTHER_VALUE: &[u8] = b"distinctive-other-value-9876543210";
const PLAIN_VALUE: &[u8] = b"distinctive-plain-value-abcdefghij";
const MARKER: &str = "OXNATIVE";

fn definitions() -> Vec<ColumnFamilyDefinition> {
    [PLAIN, SECRET, OTHER_SECRET]
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
        vault_column_families: vec![SECRET, OTHER_SECRET],
    }
}

fn open_vault_with(path: &Path, key: [u8; 32]) -> Result<Db, KvError> {
    Db::open_read_write(
        path,
        definitions(),
        DbOptions {
            vault: Some(vault_options(key)),
            ..DbOptions::default()
        },
    )
}

fn open_vault(path: &Path) -> Result<Db, KvError> {
    open_vault_with(path, KEY_A)
}

fn open_vault_read_only(path: &Path, key: [u8; 32]) -> Result<Db, KvError> {
    Db::open_read_only_with_vault(path, definitions(), vault_options(key))
}

fn open_plain(path: &Path) -> Result<Db, KvError> {
    Db::open_read_write(path, definitions(), DbOptions::default())
}

fn get_vec(db: &Db, cf: &ColumnFamily, key: &[u8]) -> Result<Option<Vec<u8>>, KvError> {
    Ok(db.get(cf, key)?.map(Vec::from))
}

fn scan(db: &Db, cf: &ColumnFamily) -> Result<Vec<Vec<u8>>, KvError> {
    let snapshot = db.snapshot();
    let mut iter = snapshot.iter(cf);
    let mut keys = Vec::new();
    while let Some(key) = iter.key() {
        keys.push(key.to_vec());
        iter.next();
    }
    iter.status()?;
    Ok(keys)
}

fn is_corruption<T>(result: &Result<T, KvError>) -> bool {
    matches!(result, Err(KvError::Corruption(_)))
}

/// Reads the raw stored bytes of a key, bypassing oxnative (database closed).
fn raw_get(path: &Path, cf: &str, key: &[u8]) -> Result<Option<Vec<u8>>, Box<dyn Error>> {
    let db = fjall::Database::builder(path.join("fjall")).open()?;
    let keyspace = db.keyspace(cf, KeyspaceCreateOptions::default)?;
    Ok(keyspace.get(key)?.map(|v| v.to_vec()))
}

fn raw_set(path: &Path, cf: &str, key: &[u8], value: &[u8]) -> TestResult {
    let db = fjall::Database::builder(path.join("fjall")).open()?;
    let keyspace = db.keyspace(cf, KeyspaceCreateOptions::default)?;
    keyspace.insert(key, value)?;
    db.persist(fjall::PersistMode::SyncAll)?;
    Ok(())
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// Every file under `dir` (recursively) that contains `needle`.
fn files_containing(dir: &Path, needle: &[u8]) -> Result<Vec<PathBuf>, Box<dyn Error>> {
    let mut found = Vec::new();
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            found.extend(files_containing(&path, needle)?);
        } else if contains(&fs::read(&path)?, needle) {
            found.push(path);
        }
    }
    Ok(found)
}

fn file_count(dir: &Path) -> Result<usize, Box<dyn Error>> {
    let mut count = 0;
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        count += if path.is_dir() { file_count(&path)? } else { 1 };
    }
    Ok(count)
}

/// A flushed vault database with data in the three column families, closed.
fn populated(path: &Path) -> TestResult {
    let db = open_vault(path)?;
    let secret = db.column_family(SECRET)?;
    let other = db.column_family(OTHER_SECRET)?;
    let plain = db.column_family(PLAIN)?;
    let mut transaction = db.start_transaction()?;
    transaction.insert(&secret, b"k1", SECRET_VALUE);
    transaction.insert(&secret, b"k2", b"second secret");
    transaction.insert(&secret, b"p/1", b"prefixed one");
    transaction.insert(&secret, b"p/2", b"prefixed two");
    transaction.insert_empty(&secret, b"empty");
    transaction.insert(&other, b"k1", OTHER_VALUE);
    transaction.insert(&plain, b"k1", PLAIN_VALUE);
    transaction.commit()?;
    db.flush()?;
    Ok(())
}

/// Writes a sealed file for `cf` with the given entries, returns its path.
fn sealed_file(db: &Db, cf: &ColumnFamily, entries: &[(&[u8], &[u8])]) -> Result<PathBuf, KvError> {
    let mut file = db.new_sst_file_for(cf)?;
    for (key, value) in entries {
        file.insert(key, value)?;
    }
    file.finish()
}

fn assert_nothing_visible(db: &Db, cfs: &[&ColumnFamily], keys: &[&[u8]]) -> TestResult {
    for cf in cfs {
        for key in keys {
            assert_eq!(
                get_vec(db, cf, key)?,
                None,
                "a refused load must leave nothing behind"
            );
        }
    }
    Ok(())
}

// ===================== backup() =====================

#[test]
fn backup_of_a_vault_database_opens_with_the_key_and_has_the_same_data() -> TestResult {
    let dir = TempDir::new()?;
    populated(&dir.path().join("db"))?;
    let target = dir.path().join("backup");
    {
        let db = open_vault(&dir.path().join("db"))?;
        db.backup(&target)
            .map_err(|e| format!("backup of a vault database must succeed: {e}"))?;
    }
    let backup = open_vault(&target)?;
    let secret = backup.column_family(SECRET)?;
    let other = backup.column_family(OTHER_SECRET)?;
    let plain = backup.column_family(PLAIN)?;
    assert_eq!(
        get_vec(&backup, &secret, b"k1")?.as_deref(),
        Some(SECRET_VALUE)
    );
    assert_eq!(
        get_vec(&backup, &secret, b"k2")?.as_deref(),
        Some(&b"second secret"[..])
    );
    assert_eq!(
        get_vec(&backup, &secret, b"empty")?.as_deref(),
        Some(&b""[..])
    );
    assert_eq!(
        get_vec(&backup, &other, b"k1")?.as_deref(),
        Some(OTHER_VALUE)
    );
    assert_eq!(
        get_vec(&backup, &plain, b"k1")?.as_deref(),
        Some(PLAIN_VALUE)
    );
    assert_eq!(
        scan(&backup, &secret)?,
        vec![
            b"empty".to_vec(),
            b"k1".to_vec(),
            b"k2".to_vec(),
            b"p/1".to_vec(),
            b"p/2".to_vec()
        ],
        "keys stay ordered in the backup"
    );
    let snapshot = backup.snapshot();
    let mut iter = snapshot.scan_prefix(&secret, b"p/");
    let mut prefixed = 0;
    while iter.key().is_some() {
        prefixed += 1;
        iter.next();
    }
    iter.status()?;
    assert_eq!(prefixed, 2, "prefix scans work on a restored vault CF");
    Ok(())
}

#[test]
fn backup_of_a_vault_database_opens_read_only_with_the_key() -> TestResult {
    let dir = TempDir::new()?;
    populated(&dir.path().join("db"))?;
    let target = dir.path().join("backup");
    open_vault(&dir.path().join("db"))?.backup(&target)?;
    let backup = open_vault_read_only(&target, KEY_A)?;
    let secret = backup.column_family(SECRET)?;
    assert_eq!(
        get_vec(&backup, &secret, b"k1")?.as_deref(),
        Some(SECRET_VALUE)
    );
    Ok(())
}

#[test]
fn backup_directory_and_raw_values_contain_no_plaintext() -> TestResult {
    let dir = TempDir::new()?;
    populated(&dir.path().join("db"))?;
    let target = dir.path().join("backup");
    open_vault(&dir.path().join("db"))?.backup(&target)?;
    for secret in [SECRET_VALUE, OTHER_VALUE, b"second secret", b"prefixed one"] {
        assert_eq!(
            files_containing(&target, secret)?,
            Vec::<PathBuf>::new(),
            "vault plaintext {:?} found in the backup",
            String::from_utf8_lossy(secret)
        );
    }
    assert!(
        !files_containing(&target, PLAIN_VALUE)?.is_empty(),
        "control: plain CF values are stored as before, so the grep is meaningful"
    );
    let sealed = raw_get(&target, SECRET, b"k1")?.ok_or("missing k1 in backup")?;
    assert_eq!(sealed.len(), SECRET_VALUE.len() + SEAL_OVERHEAD);
    assert_eq!(
        open(&VaultKey::new(KEY_A), SECRET, b"k1", &sealed)?,
        SECRET_VALUE,
        "backup values are sealed for their own column family and key"
    );
    let plain = raw_get(&target, PLAIN, b"k1")?.ok_or("missing plain k1")?;
    assert_eq!(plain, PLAIN_VALUE, "plain CFs are copied unchanged");
    Ok(())
}

#[test]
fn backup_carries_the_vault_marker_and_key_check() -> TestResult {
    let dir = TempDir::new()?;
    populated(&dir.path().join("db"))?;
    let target = dir.path().join("backup");
    open_vault(&dir.path().join("db"))?.backup(&target)?;
    assert_eq!(
        fs::read_to_string(target.join(MARKER))?,
        fs::read_to_string(dir.path().join("db").join(MARKER))?,
        "the backup marker has the vault flag like the source"
    );
    let check = raw_get(&target, META_KEYSPACE, VAULT_CHECK_KEY)?
        .ok_or("the backup has no vault key check")?;
    verify_check(&VaultKey::new(KEY_A), &check)?;
    Ok(())
}

#[test]
fn backup_of_a_vault_database_refuses_to_open_without_or_with_the_wrong_key() -> TestResult {
    let dir = TempDir::new()?;
    populated(&dir.path().join("db"))?;
    let target = dir.path().join("backup");
    open_vault(&dir.path().join("db"))?.backup(&target)?;
    assert!(open_plain(&target).is_err(), "no key: fail closed");
    assert!(
        Db::open_read_only(&target, definitions()).is_err(),
        "no key, read-only: fail closed"
    );
    assert!(open_vault_with(&target, KEY_B).is_err(), "wrong key");
    assert!(open_vault_read_only(&target, KEY_B).is_err(), "wrong key");
    // None of the refused opens damaged the backup
    let backup = open_vault(&target)?;
    let secret = backup.column_family(SECRET)?;
    assert_eq!(
        get_vec(&backup, &secret, b"k1")?.as_deref(),
        Some(SECRET_VALUE)
    );
    Ok(())
}

#[test]
fn tampering_with_a_backup_is_detected() -> TestResult {
    let dir = TempDir::new()?;
    populated(&dir.path().join("db"))?;
    let target = dir.path().join("backup");
    open_vault(&dir.path().join("db"))?.backup(&target)?;
    // Swap two ciphertexts of the backup
    let first = raw_get(&target, SECRET, b"k1")?.ok_or("missing k1")?;
    let second = raw_get(&target, SECRET, b"k2")?.ok_or("missing k2")?;
    assert!(first.len() > SEAL_OVERHEAD, "backup values must be sealed");
    raw_set(&target, SECRET, b"k1", &second)?;
    raw_set(&target, SECRET, b"k2", &first)?;
    // And move one to another vault CF
    let other = raw_get(&target, OTHER_SECRET, b"k1")?.ok_or("missing other k1")?;
    raw_set(&target, SECRET, b"p/1", &other)?;
    let backup = open_vault(&target)?;
    let secret = backup.column_family(SECRET)?;
    assert!(is_corruption(&backup.get(&secret, b"k1")), "swapped");
    assert!(is_corruption(&backup.get(&secret, b"k2")), "swapped");
    assert!(is_corruption(&backup.get(&secret, b"p/1")), "moved");
    assert_eq!(
        get_vec(&backup, &secret, b"p/2")?.as_deref(),
        Some(&b"prefixed two"[..]),
        "untouched values still open"
    );
    Ok(())
}

#[test]
fn backup_does_not_launder_a_tampered_value() -> TestResult {
    let dir = TempDir::new()?;
    populated(&dir.path().join("db"))?;
    raw_set(&dir.path().join("db"), SECRET, b"k1", b"planted plaintext")?;
    let target = dir.path().join("backup");
    let db = open_vault(&dir.path().join("db"))?;
    // Either the backup fails (and leaves nothing), or it copies the bad value
    // as is so that reading it still fails. It never turns it into valid data.
    if db.backup(&target).is_ok() {
        drop(db);
        let backup = open_vault(&target)?;
        let secret = backup.column_family(SECRET)?;
        assert!(is_corruption(&backup.get(&secret, b"k1")));
    } else {
        assert!(!target.exists(), "no half-made backup is left behind");
    }
    Ok(())
}

#[test]
fn backup_can_be_taken_from_a_read_only_vault_handle() -> TestResult {
    let dir = TempDir::new()?;
    populated(&dir.path().join("db"))?;
    let target = dir.path().join("backup");
    open_vault_read_only(&dir.path().join("db"), KEY_A)?.backup(&target)?;
    assert_eq!(
        files_containing(&target, SECRET_VALUE)?,
        Vec::<PathBuf>::new()
    );
    let backup = open_vault(&target)?;
    let secret = backup.column_family(SECRET)?;
    assert_eq!(
        get_vec(&backup, &secret, b"k1")?.as_deref(),
        Some(SECRET_VALUE)
    );
    Ok(())
}

#[test]
fn backup_is_a_point_in_time_copy() -> TestResult {
    let dir = TempDir::new()?;
    populated(&dir.path().join("db"))?;
    let target = dir.path().join("backup");
    let db = open_vault(&dir.path().join("db"))?;
    db.backup(&target)?;
    let secret = db.column_family(SECRET)?;
    db.insert(&secret, b"after", b"written after the backup")?;
    let backup = open_vault(&target)?;
    let backup_secret = backup.column_family(SECRET)?;
    assert_eq!(get_vec(&backup, &backup_secret, b"after")?, None);
    assert_eq!(
        get_vec(&db, &secret, b"after")?.as_deref(),
        Some(&b"written after the backup"[..])
    );
    Ok(())
}

#[test]
fn backup_refuses_an_existing_target_and_leaves_it_alone() -> TestResult {
    let dir = TempDir::new()?;
    populated(&dir.path().join("db"))?;
    let target = dir.path().join("backup");
    fs::create_dir_all(&target)?;
    fs::write(target.join("keep"), b"mine")?;
    let db = open_vault(&dir.path().join("db"))?;
    assert!(db.backup(&target).is_err());
    assert_eq!(fs::read(target.join("keep"))?, b"mine");
    assert_eq!(file_count(&target)?, 1);
    Ok(())
}

#[test]
fn backup_of_a_plain_database_is_unchanged_when_vault_is_compiled_in() -> TestResult {
    let dir = TempDir::new()?;
    let source = dir.path().join("db");
    {
        let db = open_plain(&source)?;
        let plain = db.column_family(PLAIN)?;
        db.insert(&plain, b"k", PLAIN_VALUE)?;
        db.flush()?;
    }
    let target = dir.path().join("backup");
    open_plain(&source)?.backup(&target)?;
    assert_eq!(
        fs::read_to_string(target.join(MARKER))?,
        "oxnative 1\nengine fjall 3\n",
        "a plain backup has no vault flag"
    );
    assert!(
        raw_get(&target, META_KEYSPACE, VAULT_CHECK_KEY)?.is_none(),
        "a plain backup has no key check"
    );
    assert_eq!(
        raw_get(&target, PLAIN, b"k")?.as_deref(),
        Some(PLAIN_VALUE),
        "plain values are copied byte for byte"
    );
    let backup = open_plain(&target)?;
    let plain = backup.column_family(PLAIN)?;
    assert_eq!(
        get_vec(&backup, &plain, b"k")?.as_deref(),
        Some(PLAIN_VALUE)
    );
    assert!(
        open_vault(&target).is_err(),
        "a plain backup is never opened as vaulted"
    );
    Ok(())
}

// ===================== bulk-load files =====================

#[test]
fn sealed_file_hides_values_and_its_ingestion_round_trips() -> TestResult {
    let dir = TempDir::new()?;
    let db = open_vault(dir.path())?;
    let secret = db.column_family(SECRET)?;
    let path = sealed_file(
        &db,
        &secret,
        &[(b"a", SECRET_VALUE), (b"b", b""), (b"c", OTHER_VALUE)],
    )?;
    let bytes = fs::read(&path)?;
    assert!(!contains(&bytes, SECRET_VALUE), "plaintext in the file");
    assert!(!contains(&bytes, OTHER_VALUE), "plaintext in the file");

    db.insert_stt_files(&[(secret.clone(), path)])
        .map_err(|e| format!("a sealed file must load into its vault CF: {e}"))?;
    assert_eq!(get_vec(&db, &secret, b"a")?.as_deref(), Some(SECRET_VALUE));
    assert_eq!(get_vec(&db, &secret, b"b")?.as_deref(), Some(&b""[..]));
    assert_eq!(get_vec(&db, &secret, b"c")?.as_deref(), Some(OTHER_VALUE));
    assert_eq!(
        scan(&db, &secret)?,
        vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]
    );
    Ok(())
}

#[test]
fn control_a_plain_writer_leaves_plaintext_in_its_file() -> TestResult {
    let dir = TempDir::new()?;
    let db = open_vault(dir.path())?;
    let plain = db.column_family(PLAIN)?;
    let mut file = db.new_sst_file_for(&plain)?;
    file.insert(b"k", PLAIN_VALUE)?;
    let path = file.finish()?;
    assert!(
        contains(&fs::read(&path)?, PLAIN_VALUE),
        "plain CF files are unchanged"
    );
    db.insert_stt_files(&[(plain.clone(), path)])?;
    assert_eq!(get_vec(&db, &plain, b"k")?.as_deref(), Some(PLAIN_VALUE));
    Ok(())
}

#[test]
fn plain_file_format_is_identical_for_new_sst_file_and_for_a_plain_cf() -> TestResult {
    let dir = TempDir::new()?;
    let db = open_vault(dir.path())?;
    let plain = db.column_family(PLAIN)?;
    let mut old = db.new_sst_file()?;
    old.insert(b"k", PLAIN_VALUE)?;
    let mut new = db.new_sst_file_for(&plain)?;
    new.insert(b"k", PLAIN_VALUE)?;
    assert_eq!(
        fs::read(old.finish()?)?,
        fs::read(new.finish()?)?,
        "plain bulk-load files stay byte-identical"
    );
    Ok(())
}

#[test]
fn ingested_vault_values_are_sealed_on_disk_and_survive_compaction_and_reopen() -> TestResult {
    let dir = TempDir::new()?;
    {
        let db = open_vault(dir.path())?;
        let secret = db.column_family(SECRET)?;
        let path = sealed_file(&db, &secret, &[(b"a", SECRET_VALUE), (b"b", OTHER_VALUE)])?;
        db.insert_stt_files(&[(secret.clone(), path)])?;
        db.flush()?;
        db.compact(&secret)?;
        assert_eq!(get_vec(&db, &secret, b"a")?.as_deref(), Some(SECRET_VALUE));
    }
    assert_eq!(
        files_containing(&dir.path().join("fjall"), SECRET_VALUE)?,
        Vec::<PathBuf>::new(),
        "no plaintext in the database files after ingestion"
    );
    let sealed = raw_get(dir.path(), SECRET, b"a")?.ok_or("missing a")?;
    assert_eq!(
        open(&VaultKey::new(KEY_A), SECRET, b"a", &sealed)?,
        SECRET_VALUE,
        "stored value is sealed for its CF and key"
    );
    let db = open_vault(dir.path())?;
    let secret = db.column_family(SECRET)?;
    assert_eq!(get_vec(&db, &secret, b"b")?.as_deref(), Some(OTHER_VALUE));
    Ok(())
}

#[test]
fn sealed_files_load_into_several_vault_cfs_and_plain_ones_in_one_call() -> TestResult {
    let dir = TempDir::new()?;
    let db = open_vault(dir.path())?;
    let secret = db.column_family(SECRET)?;
    let other = db.column_family(OTHER_SECRET)?;
    let plain = db.column_family(PLAIN)?;
    let first = sealed_file(&db, &secret, &[(b"k", SECRET_VALUE)])?;
    let second = sealed_file(&db, &other, &[(b"k", OTHER_VALUE)])?;
    let mut writer = db.new_sst_file_for(&plain)?;
    writer.insert(b"k", PLAIN_VALUE)?;
    let third = writer.finish()?;
    db.insert_stt_files(&[
        (secret.clone(), first),
        (other.clone(), second),
        (plain.clone(), third),
    ])?;
    assert_eq!(get_vec(&db, &secret, b"k")?.as_deref(), Some(SECRET_VALUE));
    assert_eq!(get_vec(&db, &other, b"k")?.as_deref(), Some(OTHER_VALUE));
    assert_eq!(get_vec(&db, &plain, b"k")?.as_deref(), Some(PLAIN_VALUE));
    Ok(())
}

#[test]
fn overlapping_sealed_files_merge_with_the_last_file_winning() -> TestResult {
    let dir = TempDir::new()?;
    let db = open_vault(dir.path())?;
    let secret = db.column_family(SECRET)?;
    let first = sealed_file(&db, &secret, &[(b"a", b"first a"), (b"b", b"first b")])?;
    let second = sealed_file(&db, &secret, &[(b"b", b"second b"), (b"c", b"second c")])?;
    db.insert_stt_files(&[(secret.clone(), first), (secret.clone(), second)])?;
    assert_eq!(
        get_vec(&db, &secret, b"a")?.as_deref(),
        Some(&b"first a"[..])
    );
    assert_eq!(
        get_vec(&db, &secret, b"b")?.as_deref(),
        Some(&b"second b"[..])
    );
    assert_eq!(
        get_vec(&db, &secret, b"c")?.as_deref(),
        Some(&b"second c"[..])
    );
    Ok(())
}

#[test]
fn ingestion_overrides_existing_values_of_a_vault_cf() -> TestResult {
    let dir = TempDir::new()?;
    let db = open_vault(dir.path())?;
    let secret = db.column_family(SECRET)?;
    db.insert(&secret, b"k", b"old")?;
    db.insert(&secret, b"gone", b"removed later")?;
    let path = sealed_file(&db, &secret, &[(b"gone", b"back"), (b"k", b"new")])?;
    db.insert_stt_files(&[(secret.clone(), path)])?;
    assert_eq!(get_vec(&db, &secret, b"k")?.as_deref(), Some(&b"new"[..]));
    assert_eq!(
        get_vec(&db, &secret, b"gone")?.as_deref(),
        Some(&b"back"[..])
    );
    Ok(())
}

#[test]
fn sealed_writer_still_enforces_strictly_increasing_keys() -> TestResult {
    let dir = TempDir::new()?;
    let db = open_vault(dir.path())?;
    let secret = db.column_family(SECRET)?;
    let mut file = db.new_sst_file_for(&secret)?;
    file.insert(b"b", b"v")?;
    assert!(file.insert(b"b", b"v").is_err(), "duplicate key");
    assert!(file.insert(b"a", b"v").is_err(), "decreasing key");
    Ok(())
}

#[test]
fn sealed_values_use_a_fresh_nonce_each() -> TestResult {
    let dir = TempDir::new()?;
    let db = open_vault(dir.path())?;
    let secret = db.column_family(SECRET)?;
    let first = fs::read(sealed_file(&db, &secret, &[(b"k", SECRET_VALUE)])?)?;
    let second = fs::read(sealed_file(&db, &secret, &[(b"k", SECRET_VALUE)])?)?;
    assert_ne!(first, second, "identical content must not seal identically");
    assert!(
        first.len() >= SECRET_VALUE.len() + SEAL_OVERHEAD,
        "the file holds sealed values"
    );
    Ok(())
}

#[test]
fn sealed_file_is_refused_in_the_wrong_column_family() -> TestResult {
    let dir = TempDir::new()?;
    let db = open_vault(dir.path())?;
    let secret = db.column_family(SECRET)?;
    let other = db.column_family(OTHER_SECRET)?;
    let plain = db.column_family(PLAIN)?;
    let path = sealed_file(&db, &secret, &[(b"k", SECRET_VALUE)])?;
    let to_other = db.insert_stt_files(&[(other.clone(), path.clone())]);
    assert!(to_other.is_err(), "another vault CF must refuse the file");
    let to_plain = db.insert_stt_files(&[(plain.clone(), path.clone())]);
    assert!(
        to_plain.is_err(),
        "a plain CF must refuse a sealed file (ciphertext as plain data)"
    );
    let mixed = db.insert_stt_files(&[(secret.clone(), path.clone()), (other.clone(), path)]);
    assert!(mixed.is_err(), "one bad pair fails the whole load");
    assert_nothing_visible(&db, &[&secret, &other, &plain], &[b"k"])?;
    db.flush()?;
    drop((db, secret, other, plain));
    let db = open_vault(dir.path())?;
    let secret = db.column_family(SECRET)?;
    assert_eq!(
        get_vec(&db, &secret, b"k")?,
        None,
        "still nothing after reopen"
    );
    Ok(())
}

#[test]
fn unsealed_file_is_refused_in_a_vault_cf() -> TestResult {
    let dir = TempDir::new()?;
    let db = open_vault(dir.path())?;
    let secret = db.column_family(SECRET)?;
    let mut file = db.new_sst_file()?;
    file.insert(b"k", SECRET_VALUE)?;
    let result = db.insert_stt_files(&[(secret.clone(), file.finish()?)]);
    assert!(result.is_err(), "plain values must never enter a vault CF");
    assert_nothing_visible(&db, &[&secret], &[b"k"])
}

#[test]
fn file_sealed_with_another_key_never_yields_data() -> TestResult {
    let other_dir = TempDir::new()?;
    let path = {
        let db = open_vault_with(other_dir.path(), KEY_B)?;
        let secret = db.column_family(SECRET)?;
        sealed_file(&db, &secret, &[(b"k", SECRET_VALUE)])?
    };
    let dir = TempDir::new()?;
    let db = open_vault(dir.path())?;
    let secret = db.column_family(SECRET)?;
    match db.insert_stt_files(&[(secret.clone(), path)]) {
        Err(_) => assert_nothing_visible(&db, &[&secret], &[b"k"])?,
        Ok(()) => assert!(
            is_corruption(&db.get(&secret, b"k")),
            "a value sealed under another key must not open"
        ),
    }
    Ok(())
}

#[test]
fn tampered_sealed_file_fails_closed() -> TestResult {
    let dir = TempDir::new()?;
    let db = open_vault(dir.path())?;
    let secret = db.column_family(SECRET)?;
    let path = sealed_file(
        &db,
        &secret,
        &[(b"a", SECRET_VALUE), (b"b", OTHER_VALUE), (b"c", b"third")],
    )?;
    let mut bytes = fs::read(&path)?;
    assert!(bytes.len() > SEAL_OVERHEAD, "the file holds sealed values");
    // Flip one bit inside the last sealed value
    let last = bytes.len() - 3;
    bytes[last] ^= 1;
    fs::write(&path, &bytes)?;
    match db.insert_stt_files(&[(secret.clone(), path)]) {
        Err(_) => assert_nothing_visible(&db, &[&secret], &[b"a", b"b", b"c"])?,
        Ok(()) => assert!(
            is_corruption(&db.get(&secret, b"c")),
            "a tampered sealed value must not open"
        ),
    }
    Ok(())
}

#[test]
fn truncated_sealed_file_is_refused_and_leaves_nothing() -> TestResult {
    for cut in [1, 10] {
        let dir = TempDir::new()?;
        let db = open_vault(dir.path())?;
        let secret = db.column_family(SECRET)?;
        let path = sealed_file(
            &db,
            &secret,
            &[(b"a", SECRET_VALUE), (b"b", OTHER_VALUE), (b"c", b"third")],
        )?;
        let mut bytes = fs::read(&path)?;
        assert!(bytes.len() > SEAL_OVERHEAD, "the file holds sealed values");
        bytes.truncate(bytes.len() - cut);
        fs::write(&path, &bytes)?;
        assert!(
            db.insert_stt_files(&[(secret.clone(), path)]).is_err(),
            "file cut by {cut} bytes must be refused"
        );
        assert_nothing_visible(&db, &[&secret], &[b"a", b"b", b"c"])?;
    }
    Ok(())
}

#[test]
fn sealed_file_with_swapped_values_is_detected() -> TestResult {
    // Swap the keys of the two records in place (the private format keeps the
    // `[u32 LE length][key]` framing). Values are bound to their keys, so the
    // swapped records must not open.
    let dir = TempDir::new()?;
    let db = open_vault(dir.path())?;
    let secret = db.column_family(SECRET)?;
    let path = sealed_file(
        &db,
        &secret,
        &[(b"a", b"same-length-1"), (b"b", b"same-length-2")],
    )?;
    let mut bytes = fs::read(&path)?;
    let a = bytes
        .windows(5)
        .position(|w| w == [1, 0, 0, 0, b'a'])
        .ok_or("key a not found in the file")?;
    let b = bytes
        .windows(5)
        .position(|w| w == [1, 0, 0, 0, b'b'])
        .ok_or("key b not found in the file")?;
    bytes[a + 4] = b'b';
    bytes[b + 4] = b'a';
    fs::write(&path, &bytes)?;
    if db.insert_stt_files(&[(secret.clone(), path)]).is_err() {
        assert_nothing_visible(&db, &[&secret], &[b"a", b"b"])?;
    } else {
        assert!(is_corruption(&db.get(&secret, b"a")), "swapped record");
        assert!(is_corruption(&db.get(&secret, b"b")), "swapped record");
    }
    Ok(())
}

#[test]
fn interrupted_sealed_bulk_load_is_completed_at_the_next_open() -> TestResult {
    let dir = TempDir::new()?;
    {
        let db = open_vault(dir.path())?;
        let secret = db.column_family(SECRET)?;
        let path = sealed_file(&db, &secret, &[(b"a", SECRET_VALUE), (b"b", OTHER_VALUE)])?;
        // The load record is durable but the files were not ingested yet
        let files = [(&secret, vec![path])];
        db.inner.write_bulk_load_record(files.iter())?;
    }
    let db = open_vault(dir.path())
        .map_err(|e| format!("recovery of a sealed bulk load must succeed: {e}"))?;
    let secret = db.column_family(SECRET)?;
    assert_eq!(get_vec(&db, &secret, b"a")?.as_deref(), Some(SECRET_VALUE));
    assert_eq!(get_vec(&db, &secret, b"b")?.as_deref(), Some(OTHER_VALUE));
    drop((db, secret));
    assert_eq!(
        files_containing(&dir.path().join("fjall"), SECRET_VALUE)?,
        Vec::<PathBuf>::new(),
        "recovery keeps values sealed"
    );
    Ok(())
}

#[test]
fn sealed_files_load_and_read_through_a_read_only_vault_handle() -> TestResult {
    let dir = TempDir::new()?;
    {
        let db = open_vault(dir.path())?;
        let secret = db.column_family(SECRET)?;
        let path = sealed_file(&db, &secret, &[(b"k", SECRET_VALUE)])?;
        db.insert_stt_files(&[(secret, path)])?;
        db.flush()?;
    }
    let reader = open_vault_read_only(dir.path(), KEY_A)?;
    let secret = reader.column_family(SECRET)?;
    assert_eq!(
        get_vec(&reader, &secret, b"k")?.as_deref(),
        Some(SECRET_VALUE)
    );
    assert!(
        reader.new_sst_file_for(&secret).is_err(),
        "a read-only handle cannot create bulk-load files"
    );
    Ok(())
}
