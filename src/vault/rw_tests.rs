//! Transparent encrypted reads and writes on vault column families (#6).

#![expect(clippy::panic_in_result_fn, reason = "assertions in tests")]

use super::*;
use crate::{ColumnFamily, ColumnFamilyDefinition, Db, DbOptions, Iter};
use fjall::KeyspaceCreateOptions;
use std::error::Error;
use std::path::Path;
use tempfile::TempDir;

type TestResult = Result<(), Box<dyn Error>>;

const KEY_A: [u8; 32] = [0x11; 32];
const PLAIN: &str = "plain";
const SECRET: &str = "secret";
const OTHER_SECRET: &str = "other_secret";
const VALUE: &[u8] = b"distinctive-plaintext-value-0123456789";

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

fn vault_options() -> VaultOptions {
    VaultOptions {
        key: VaultKey::new(KEY_A),
        vault_column_families: vec![SECRET, OTHER_SECRET],
    }
}

fn open_vault(path: &Path) -> Result<Db, KvError> {
    Db::open_read_write(
        path,
        definitions(),
        DbOptions {
            vault: Some(vault_options()),
            ..DbOptions::default()
        },
    )
}

fn open_plain_db(path: &Path) -> Result<Db, KvError> {
    Db::open_read_write(path, definitions(), DbOptions::default())
}

fn is_corruption<T>(result: &Result<T, KvError>) -> bool {
    matches!(result, Err(KvError::Corruption(_)))
}

fn entries(mut iter: Iter<'_>) -> Result<Vec<Vec<u8>>, KvError> {
    let mut keys = Vec::new();
    while let Some(key) = iter.key() {
        keys.push(key.to_vec());
        iter.next();
    }
    iter.status()?;
    Ok(keys)
}

fn bytes(keys: &[&str]) -> Vec<Vec<u8>> {
    keys.iter().map(|k| k.as_bytes().to_vec()).collect()
}

fn get_vec(db: &Db, cf: &ColumnFamily, key: &[u8]) -> Result<Option<Vec<u8>>, KvError> {
    Ok(db.get(cf, key)?.map(Vec::from))
}

/// Reads the raw stored bytes of a key, bypassing oxnative (database closed).
fn raw_get(path: &Path, cf: &str, key: &[u8]) -> Result<Option<Vec<u8>>, Box<dyn Error>> {
    let db = fjall::Database::builder(path.join("fjall")).open()?;
    let keyspace = db.keyspace(cf, KeyspaceCreateOptions::default)?;
    Ok(keyspace.get(key)?.map(|v| v.to_vec()))
}

/// Overwrites the raw stored bytes of a key, bypassing oxnative (database closed).
fn raw_set(path: &Path, cf: &str, key: &[u8], value: &[u8]) -> TestResult {
    let db = fjall::Database::builder(path.join("fjall")).open()?;
    let keyspace = db.keyspace(cf, KeyspaceCreateOptions::default)?;
    keyspace.insert(key, value)?;
    db.persist(fjall::PersistMode::SyncAll)?;
    Ok(())
}

/// A vault database holding `k1`/`k2` in `secret` and `k1` in `other_secret`,
/// closed again.
fn populated(path: &Path) -> TestResult {
    let db = open_vault(path)?;
    let secret = db.column_family(SECRET)?;
    let other = db.column_family(OTHER_SECRET)?;
    let mut transaction = db.start_transaction()?;
    transaction.insert(&secret, b"k1", b"first value");
    transaction.insert(&secret, b"k2", b"second value");
    transaction.insert(&other, b"k1", b"other value");
    transaction.commit()?;
    db.flush()?;
    Ok(())
}

// --- round trips ---

#[test]
fn db_insert_and_get_round_trip_on_a_vault_cf() -> TestResult {
    let dir = TempDir::new()?;
    let db = open_vault(dir.path())?;
    let secret = db.column_family(SECRET)?;
    assert_eq!(get_vec(&db, &secret, b"k")?, None, "missing key is None");
    assert!(!db.contains_key(&secret, b"k")?);
    db.insert(&secret, b"k", VALUE)?;
    assert_eq!(get_vec(&db, &secret, b"k")?.as_deref(), Some(VALUE));
    assert!(db.contains_key(&secret, b"k")?);
    Ok(())
}

#[test]
fn transaction_snapshot_and_reopen_round_trip() -> TestResult {
    let dir = TempDir::new()?;
    {
        let db = open_vault(dir.path())?;
        let secret = db.column_family(SECRET)?;
        let mut transaction = db.start_transaction()?;
        transaction.insert(&secret, b"k", VALUE);
        transaction.commit()?;
        let snapshot = db.snapshot();
        assert_eq!(
            snapshot.get(&secret, b"k")?.as_deref(),
            Some(VALUE),
            "snapshot reads decrypt"
        );
        assert!(snapshot.contains_key(&secret, b"k")?);
        db.flush()?;
    }
    let db = open_vault(dir.path())?;
    let secret = db.column_family(SECRET)?;
    assert_eq!(get_vec(&db, &secret, b"k")?.as_deref(), Some(VALUE));
    Ok(())
}

#[test]
fn empty_values_round_trip() -> TestResult {
    let dir = TempDir::new()?;
    let db = open_vault(dir.path())?;
    let secret = db.column_family(SECRET)?;
    let mut transaction = db.start_transaction()?;
    transaction.insert_empty(&secret, b"empty");
    transaction.commit()?;
    db.insert(&secret, b"empty2", b"")?;
    assert_eq!(get_vec(&db, &secret, b"empty")?.as_deref(), Some(&b""[..]));
    assert_eq!(get_vec(&db, &secret, b"empty2")?.as_deref(), Some(&b""[..]));
    Ok(())
}

#[test]
fn read_only_handle_with_the_key_reads_plaintext() -> TestResult {
    let dir = TempDir::new()?;
    populated(dir.path())?;
    let db = Db::open_read_only_with_vault(dir.path(), definitions(), vault_options())?;
    let secret = db.column_family(SECRET)?;
    assert_eq!(
        get_vec(&db, &secret, b"k1")?.as_deref(),
        Some(&b"first value"[..])
    );
    assert_eq!(
        db.snapshot().get(&secret, b"k2")?.as_deref(),
        Some(&b"second value"[..])
    );
    Ok(())
}

#[test]
fn readable_transaction_reads_its_own_writes_in_plaintext() -> TestResult {
    let dir = TempDir::new()?;
    populated(dir.path())?;
    let db = open_vault(dir.path())?;
    let secret = db.column_family(SECRET)?;
    let mut transaction = db.start_readable_transaction()?;
    transaction.insert(&secret, b"k3", b"pending value");
    transaction.insert(&secret, b"k1", b"overwritten");
    transaction.remove(&secret, b"k2");
    {
        let reader = transaction.reader();
        assert_eq!(
            reader.get(&secret, b"k3")?.as_deref(),
            Some(&b"pending value"[..]),
            "pending write is read back as plaintext"
        );
        assert_eq!(
            reader.get(&secret, b"k1")?.as_deref(),
            Some(&b"overwritten"[..])
        );
        assert!(
            reader.get(&secret, b"k2")?.is_none(),
            "pending removal wins"
        );
        assert!(!reader.contains_key(&secret, b"k2")?);
        assert_eq!(entries(reader.iter(&secret))?, bytes(&["k1", "k3"]));
        let other = db.column_family(OTHER_SECRET)?;
        assert_eq!(
            reader.get(&other, b"k1")?.as_deref(),
            Some(&b"other value"[..]),
            "committed values of an untouched vault cf still decrypt"
        );
    }
    transaction.commit()?;
    assert_eq!(
        get_vec(&db, &secret, b"k3")?.as_deref(),
        Some(&b"pending value"[..])
    );
    assert_eq!(
        get_vec(&db, &secret, b"k1")?.as_deref(),
        Some(&b"overwritten"[..])
    );
    assert_eq!(get_vec(&db, &secret, b"k2")?, None);
    Ok(())
}

#[test]
fn last_write_and_removal_win_inside_a_transaction() -> TestResult {
    let dir = TempDir::new()?;
    let db = open_vault(dir.path())?;
    let secret = db.column_family(SECRET)?;
    let mut transaction = db.start_transaction()?;
    transaction.insert(&secret, b"a", b"one");
    transaction.insert(&secret, b"a", b"two");
    transaction.insert(&secret, b"b", b"gone");
    transaction.remove(&secret, b"b");
    transaction.commit()?;
    assert_eq!(get_vec(&db, &secret, b"a")?.as_deref(), Some(&b"two"[..]));
    assert_eq!(get_vec(&db, &secret, b"b")?, None);
    Ok(())
}

#[test]
fn removals_and_range_removals_work_on_vault_cfs() -> TestResult {
    let dir = TempDir::new()?;
    let db = open_vault(dir.path())?;
    let secret = db.column_family(SECRET)?;
    let mut transaction = db.start_transaction()?;
    for key in ["a", "b", "c", "d"] {
        transaction.insert(&secret, key.as_bytes(), VALUE);
    }
    transaction.commit()?;
    let mut transaction = db.start_transaction()?;
    transaction.remove(&secret, b"a");
    transaction.remove_range(&secret, b"b", b"d");
    transaction.commit()?;
    assert_eq!(entries(db.snapshot().iter(&secret))?, bytes(&["d"]));
    assert_eq!(get_vec(&db, &secret, b"d")?.as_deref(), Some(VALUE));
    Ok(())
}

// --- ordering and scans: keys stay plaintext ---

#[test]
fn iteration_and_prefix_scans_keep_order_on_vault_cfs() -> TestResult {
    let dir = TempDir::new()?;
    let db = open_vault(dir.path())?;
    let secret = db.column_family(SECRET)?;
    let mut transaction = db.start_transaction()?;
    for key in ["b2", "a1", "b1", "c1", "a2"] {
        transaction.insert(&secret, key.as_bytes(), key.as_bytes());
    }
    transaction.commit()?;
    db.flush()?;
    let snapshot = db.snapshot();
    assert_eq!(
        entries(snapshot.iter(&secret))?,
        bytes(&["a1", "a2", "b1", "b2", "c1"])
    );
    assert_eq!(
        entries(snapshot.scan_prefix(&secret, b"b"))?,
        bytes(&["b1", "b2"])
    );
    assert_eq!(snapshot.len(&secret)?, 5);
    assert!(!snapshot.is_empty(&secret)?);
    assert!(entries(snapshot.scan_prefix(&secret, b"z"))?.is_empty());
    Ok(())
}

// --- the stored bytes ---

#[test]
fn vault_values_are_sealed_on_disk_and_keys_stay_plaintext() -> TestResult {
    let dir = TempDir::new()?;
    {
        let db = open_vault(dir.path())?;
        let secret = db.column_family(SECRET)?;
        let mut transaction = db.start_transaction()?;
        transaction.insert(&secret, b"record-key", VALUE);
        transaction.commit()?;
        db.insert(&secret, b"direct-key", VALUE)?;
        db.flush()?;
    }
    for key in [&b"record-key"[..], b"direct-key"] {
        let raw = raw_get(dir.path(), SECRET, key)?.ok_or("keys must be stored in plaintext")?;
        assert_eq!(
            raw.len(),
            VALUE.len() + SEAL_OVERHEAD,
            "stored value is the sealed form of the plaintext"
        );
        assert!(
            !raw.windows(VALUE.len()).any(|w| w == VALUE),
            "plaintext must not be stored"
        );
        let plaintext = open(&VaultKey::new(KEY_A), SECRET, key, &raw)?;
        assert_eq!(plaintext, VALUE, "bound to the cf name and the record key");
    }
    Ok(())
}

#[test]
fn plaintext_never_reaches_any_file_of_a_vault_database() -> TestResult {
    let dir = TempDir::new()?;
    {
        let db = open_vault(dir.path())?;
        let secret = db.column_family(SECRET)?;
        db.insert(&secret, b"k", VALUE)?;
        let mut transaction = db.start_transaction()?;
        transaction.insert(&secret, b"k2", VALUE);
        transaction.commit()?;
        db.flush()?;
    }
    assert!(
        !files_contain(dir.path(), VALUE)?,
        "neither the journal nor the tables may hold the plaintext value"
    );
    Ok(())
}

fn files_contain(dir: &Path, needle: &[u8]) -> Result<bool, Box<dyn Error>> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            if files_contain(&path, needle)? {
                return Ok(true);
            }
        } else if std::fs::read(&path)?
            .windows(needle.len())
            .any(|window| window == needle)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

#[test]
fn plain_cfs_of_a_vault_database_are_stored_unchanged() -> TestResult {
    let dir = TempDir::new()?;
    {
        let db = open_vault(dir.path())?;
        let plain = db.column_family(PLAIN)?;
        let mut transaction = db.start_transaction()?;
        transaction.insert(&plain, b"k", VALUE);
        transaction.commit()?;
        db.insert(&plain, b"k2", VALUE)?;
        assert_eq!(get_vec(&db, &plain, b"k")?.as_deref(), Some(VALUE));
        db.flush()?;
    }
    assert_eq!(raw_get(dir.path(), PLAIN, b"k")?.as_deref(), Some(VALUE));
    assert_eq!(raw_get(dir.path(), PLAIN, b"k2")?.as_deref(), Some(VALUE));
    Ok(())
}

#[test]
fn plain_and_vault_cfs_can_be_written_in_one_atomic_transaction() -> TestResult {
    let dir = TempDir::new()?;
    {
        let db = open_vault(dir.path())?;
        let plain = db.column_family(PLAIN)?;
        let secret = db.column_family(SECRET)?;
        let mut transaction = db.start_transaction()?;
        transaction.insert(&plain, b"k", b"visible");
        transaction.insert(&secret, b"k", b"hidden");
        transaction.commit()?;
        assert_eq!(
            get_vec(&db, &plain, b"k")?.as_deref(),
            Some(&b"visible"[..])
        );
        assert_eq!(
            get_vec(&db, &secret, b"k")?.as_deref(),
            Some(&b"hidden"[..])
        );
        db.flush()?;
    }
    assert_eq!(
        raw_get(dir.path(), PLAIN, b"k")?.as_deref(),
        Some(&b"visible"[..])
    );
    assert_ne!(
        raw_get(dir.path(), SECRET, b"k")?.as_deref(),
        Some(&b"hidden"[..])
    );
    Ok(())
}

#[test]
fn plain_database_stores_values_exactly_as_before() -> TestResult {
    let dir = TempDir::new()?;
    {
        let db = open_plain_db(dir.path())?;
        let secret = db.column_family(SECRET)?;
        db.insert(&secret, b"k", VALUE)?;
        let mut transaction = db.start_transaction()?;
        transaction.insert(&secret, b"k2", VALUE);
        transaction.commit()?;
        assert_eq!(get_vec(&db, &secret, b"k")?.as_deref(), Some(VALUE));
        db.flush()?;
    }
    assert_eq!(raw_get(dir.path(), SECRET, b"k")?.as_deref(), Some(VALUE));
    assert_eq!(raw_get(dir.path(), SECRET, b"k2")?.as_deref(), Some(VALUE));
    Ok(())
}

#[test]
fn nonces_are_fresh_for_every_write() -> TestResult {
    let dir = TempDir::new()?;
    {
        let db = open_vault(dir.path())?;
        let secret = db.column_family(SECRET)?;
        db.insert(&secret, b"a", VALUE)?;
        db.insert(&secret, b"b", VALUE)?;
        db.flush()?;
    }
    let first = raw_get(dir.path(), SECRET, b"a")?.ok_or("missing a")?;
    let second = raw_get(dir.path(), SECRET, b"b")?.ok_or("missing b")?;
    assert_ne!(first, second, "same plaintext, different ciphertext");
    {
        let db = open_vault(dir.path())?;
        let secret = db.column_family(SECRET)?;
        db.insert(&secret, b"a", VALUE)?;
        db.flush()?;
    }
    assert_ne!(
        Some(first),
        raw_get(dir.path(), SECRET, b"a")?,
        "rewriting the same value re-encrypts it"
    );
    Ok(())
}

// --- tampering: reads fail closed ---

/// Reads `key` of `cf` through every read path and checks each one fails with
/// a corruption error.
fn assert_every_read_fails(path: &Path, cf: &'static str, key: &[u8]) -> TestResult {
    let db = open_vault(path)?;
    let column_family = db.column_family(cf)?;
    assert!(
        is_corruption(&db.get(&column_family, key)),
        "Db::get must fail with corruption"
    );
    assert!(
        is_corruption(&db.snapshot().get(&column_family, key)),
        "Reader::get must fail with corruption"
    );
    let transaction = db.start_readable_transaction()?;
    assert!(
        is_corruption(&transaction.reader().get(&column_family, key)),
        "transaction reads must fail with corruption"
    );
    Ok(())
}

#[test]
fn a_flipped_bit_anywhere_in_a_sealed_value_is_detected() -> TestResult {
    let dir = TempDir::new()?;
    populated(dir.path())?;
    let sealed = raw_get(dir.path(), SECRET, b"k1")?.ok_or("missing k1")?;
    assert!(
        sealed.len() > SEAL_OVERHEAD,
        "the value must be sealed (not stored in plaintext)"
    );
    for position in [0, 1, 2, HEADER_LEN + NONCE_LEN, sealed.len() - 1] {
        let mut tampered = sealed.clone();
        tampered[position] ^= 0x01;
        raw_set(dir.path(), SECRET, b"k1", &tampered)?;
        assert_every_read_fails(dir.path(), SECRET, b"k1")?;
    }
    raw_set(dir.path(), SECRET, b"k1", &sealed)?;
    let db = open_vault(dir.path())?;
    let secret = db.column_family(SECRET)?;
    assert_eq!(
        get_vec(&db, &secret, b"k1")?.as_deref(),
        Some(&b"first value"[..]),
        "restoring the original bytes makes the value readable again"
    );
    Ok(())
}

#[test]
fn truncated_sealed_values_are_detected() -> TestResult {
    let dir = TempDir::new()?;
    populated(dir.path())?;
    let sealed = raw_get(dir.path(), SECRET, b"k1")?.ok_or("missing k1")?;
    assert!(sealed.len() > SEAL_OVERHEAD, "the value must be sealed");
    for len in [
        0,
        1,
        HEADER_LEN,
        HEADER_LEN + NONCE_LEN,
        SEAL_OVERHEAD - 1,
        sealed.len() - 1,
    ] {
        raw_set(dir.path(), SECRET, b"k1", &sealed[..len])?;
        assert_every_read_fails(dir.path(), SECRET, b"k1")?;
    }
    Ok(())
}

#[test]
fn appended_bytes_are_detected() -> TestResult {
    let dir = TempDir::new()?;
    populated(dir.path())?;
    let mut sealed = raw_get(dir.path(), SECRET, b"k1")?.ok_or("missing k1")?;
    assert!(sealed.len() > SEAL_OVERHEAD, "the value must be sealed");
    sealed.push(0);
    raw_set(dir.path(), SECRET, b"k1", &sealed)?;
    assert_every_read_fails(dir.path(), SECRET, b"k1")
}

#[test]
fn a_plaintext_value_planted_in_a_vault_cf_is_refused() -> TestResult {
    let dir = TempDir::new()?;
    populated(dir.path())?;
    raw_set(
        dir.path(),
        SECRET,
        b"k1",
        b"planted plaintext, never sealed",
    )?;
    assert_every_read_fails(dir.path(), SECRET, b"k1")?;
    raw_set(dir.path(), SECRET, b"k1", b"")?;
    assert_every_read_fails(dir.path(), SECRET, b"k1")
}

#[test]
fn ciphertexts_swapped_between_keys_are_detected() -> TestResult {
    let dir = TempDir::new()?;
    populated(dir.path())?;
    let first = raw_get(dir.path(), SECRET, b"k1")?.ok_or("missing k1")?;
    let second = raw_get(dir.path(), SECRET, b"k2")?.ok_or("missing k2")?;
    assert!(first.len() > SEAL_OVERHEAD, "the values must be sealed");
    raw_set(dir.path(), SECRET, b"k1", &second)?;
    raw_set(dir.path(), SECRET, b"k2", &first)?;
    assert_every_read_fails(dir.path(), SECRET, b"k1")?;
    assert_every_read_fails(dir.path(), SECRET, b"k2")
}

#[test]
fn ciphertexts_moved_between_vault_cfs_are_detected() -> TestResult {
    let dir = TempDir::new()?;
    populated(dir.path())?;
    let from_secret = raw_get(dir.path(), SECRET, b"k1")?.ok_or("missing secret k1")?;
    let from_other = raw_get(dir.path(), OTHER_SECRET, b"k1")?.ok_or("missing other k1")?;
    assert!(
        from_secret.len() > SEAL_OVERHEAD,
        "the values must be sealed"
    );
    raw_set(dir.path(), OTHER_SECRET, b"k1", &from_secret)?;
    raw_set(dir.path(), SECRET, b"k1", &from_other)?;
    assert_every_read_fails(dir.path(), SECRET, b"k1")?;
    assert_every_read_fails(dir.path(), OTHER_SECRET, b"k1")
}

#[test]
fn an_old_ciphertext_replayed_under_its_own_key_still_opens() -> TestResult {
    // Documents the limit of value-only AEAD: replaying a value at its own
    // location is not detectable. Only moving it elsewhere is.
    let dir = TempDir::new()?;
    populated(dir.path())?;
    let old = raw_get(dir.path(), SECRET, b"k1")?.ok_or("missing k1")?;
    assert!(old.len() > SEAL_OVERHEAD, "the value must be sealed");
    {
        let db = open_vault(dir.path())?;
        let secret = db.column_family(SECRET)?;
        db.insert(&secret, b"k1", b"newer value")?;
        db.flush()?;
    }
    raw_set(dir.path(), SECRET, b"k1", &old)?;
    let db = open_vault(dir.path())?;
    let secret = db.column_family(SECRET)?;
    assert_eq!(
        get_vec(&db, &secret, b"k1")?.as_deref(),
        Some(&b"first value"[..])
    );
    Ok(())
}

#[test]
fn a_tampered_value_does_not_poison_other_keys_or_plain_cfs() -> TestResult {
    let dir = TempDir::new()?;
    populated(dir.path())?;
    {
        let db = open_vault(dir.path())?;
        let plain = db.column_family(PLAIN)?;
        db.insert(&plain, b"p", b"plain survives")?;
        db.flush()?;
    }
    raw_set(dir.path(), SECRET, b"k1", b"garbage")?;
    let db = open_vault(dir.path())?;
    let secret = db.column_family(SECRET)?;
    let plain = db.column_family(PLAIN)?;
    assert!(is_corruption(&db.get(&secret, b"k1")));
    assert_eq!(
        get_vec(&db, &secret, b"k2")?.as_deref(),
        Some(&b"second value"[..])
    );
    assert_eq!(
        get_vec(&db, &plain, b"p")?.as_deref(),
        Some(&b"plain survives"[..])
    );
    Ok(())
}
