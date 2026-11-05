#![expect(clippy::panic_in_result_fn, reason = "assertions in tests")]

use super::*;
use std::error::Error;
use tempfile::TempDir;

type TestResult = Result<(), Box<dyn Error>>;

fn definitions() -> Vec<ColumnFamilyDefinition> {
    ["a", "b"]
        .into_iter()
        .map(|name| ColumnFamilyDefinition {
            name,
            use_iter: true,
            min_prefix_size: 0,
            unordered_writes: false,
        })
        .collect()
}

fn open(path: &Path) -> Result<Db, KvError> {
    Db::open_read_write(path, definitions(), DbOptions::default())
}

fn keys(mut iter: Iter<'_>) -> Result<Vec<Vec<u8>>, KvError> {
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

fn commit_keys(db: &Db, cf: &ColumnFamily, keys: &[&str]) -> Result<(), KvError> {
    let mut transaction = db.start_transaction()?;
    for key in keys {
        transaction.insert(cf, key.as_bytes(), b"v");
    }
    transaction.commit()
}

#[test]
fn default_column_family_always_exists() -> TestResult {
    let dir = TempDir::new()?;
    let db = open(dir.path())?;
    db.column_family("default")?;
    assert!(matches!(
        db.column_family("missing"),
        Err(KvError::MissingColumnFamily("missing"))
    ));
    Ok(())
}

#[test]
fn snapshot_isolation_and_reopen() -> TestResult {
    let dir = TempDir::new()?;
    {
        let db = open(dir.path())?;
        let cf = db.column_family("a")?;
        let before = db.snapshot();
        commit_keys(&db, &cf, &["k1", "k2"])?;
        assert!(!before.contains_key(&cf, b"k1")?);
        assert_eq!(keys(before.iter(&cf))?, Vec::<Vec<u8>>::new());
        let after = db.snapshot();
        assert_eq!(keys(after.iter(&cf))?, bytes(&["k1", "k2"]));
        assert_eq!(after.get(&cf, b"k1")?.as_deref(), Some(&b"v"[..]));
        assert_eq!(after.len(&cf)?, 2);
    }
    let db = open(dir.path())?;
    let cf = db.column_family("a")?;
    assert_eq!(keys(db.snapshot().iter(&cf))?, bytes(&["k1", "k2"]));
    assert!(db.snapshot().is_empty(&db.column_family("b")?)?);
    Ok(())
}

#[test]
fn readable_transaction_merges_pending_writes() -> TestResult {
    let dir = TempDir::new()?;
    let db = open(dir.path())?;
    let cf = db.column_family("a")?;
    commit_keys(&db, &cf, &["a", "c", "e", "pa", "pc"])?;

    let mut transaction = db.start_readable_transaction()?;
    transaction.insert(&cf, b"b", b"new");
    transaction.remove(&cf, b"c");
    transaction.insert(&cf, b"e", b"overwritten");
    transaction.remove(&cf, b"z"); // removal of a missing key
    transaction.insert(&cf, b"pb", b"");
    {
        let reader = transaction.reader();
        assert_eq!(
            keys(reader.iter(&cf))?,
            bytes(&["a", "b", "e", "pa", "pb", "pc"])
        );
        assert_eq!(
            keys(reader.scan_prefix(&cf, b"p"))?,
            bytes(&["pa", "pb", "pc"])
        );
        assert_eq!(reader.get(&cf, b"e")?.as_deref(), Some(&b"overwritten"[..]));
        assert!(reader.get(&cf, b"c")?.is_none());
        assert!(!reader.contains_key(&cf, b"c")?);
        assert!(reader.contains_key(&cf, b"b")?);
        assert_eq!(reader.len(&cf)?, 6);
    }
    // Not visible to others before commit
    assert!(!db.snapshot().contains_key(&cf, b"b")?);
    transaction.commit()?;
    assert_eq!(
        keys(db.snapshot().iter(&cf))?,
        bytes(&["a", "b", "e", "pa", "pb", "pc"])
    );
    assert_eq!(db.get(&cf, b"e")?.as_deref(), Some(&b"overwritten"[..]));
    Ok(())
}

#[test]
fn prefix_of_max_bytes() -> TestResult {
    let dir = TempDir::new()?;
    let db = open(dir.path())?;
    let cf = db.column_family("a")?;
    let mut transaction = db.start_transaction()?;
    transaction.insert_empty(&cf, &[0xFF, 0x01]);
    transaction.insert_empty(&cf, &[0xFE, 0xFF]);
    transaction.commit()?;
    let mut transaction = db.start_readable_transaction()?;
    transaction.insert_empty(&cf, &[0xFF, 0xFF]);
    let reader = transaction.reader();
    assert_eq!(
        keys(reader.scan_prefix(&cf, &[0xFF]))?,
        vec![vec![0xFF, 0x01], vec![0xFF, 0xFF]]
    );
    assert_eq!(
        keys(reader.scan_prefix(&cf, &[0xFE]))?,
        vec![vec![0xFE, 0xFF]]
    );
    Ok(())
}

#[test]
fn remove_range_follows_write_order() -> TestResult {
    let dir = TempDir::new()?;
    let db = open(dir.path())?;
    let cf = db.column_family("a")?;
    let other = db.column_family("b")?;
    commit_keys(&db, &cf, &["a", "b", "c", "d"])?;
    commit_keys(&db, &other, &["b"])?;

    let mut transaction = db.start_transaction()?;
    transaction.insert(&cf, b"bb", b"before"); // removed by the range below
    transaction.remove_range(&cf, b"b", b"d");
    transaction.insert(&cf, b"c", b"after"); // kept
    transaction.commit()?;

    let snapshot = db.snapshot();
    assert_eq!(keys(snapshot.iter(&cf))?, bytes(&["a", "c", "d"]));
    assert_eq!(snapshot.get(&cf, b"c")?.as_deref(), Some(&b"after"[..]));
    // Other column families are untouched
    assert_eq!(keys(snapshot.iter(&other))?, bytes(&["b"]));

    // Clearing a whole column family, the way Oxigraph does
    let mut transaction = db.start_transaction()?;
    transaction.remove_range(&cf, &[], &[u8::MAX]);
    transaction.commit()?;
    assert!(db.snapshot().is_empty(&cf)?);
    Ok(())
}

fn write_file(db: &Db, entries: &[(&str, &str)]) -> Result<PathBuf, KvError> {
    let mut file = db.new_sst_file()?;
    for (key, value) in entries {
        file.insert(key.as_bytes(), value.as_bytes())?;
    }
    file.finish()
}

#[test]
fn bulk_load_merges_files_and_overrides() -> TestResult {
    let dir = TempDir::new()?;
    let db = open(dir.path())?;
    let cf = db.column_family("a")?;
    let other = db.column_family("b")?;
    commit_keys(&db, &cf, &["x"])?;
    let mut transaction = db.start_transaction()?;
    transaction.remove(&cf, b"x");
    transaction.commit()?;

    let first = write_file(&db, &[("k1", "old"), ("k3", "v"), ("x", "back")])?;
    let second = write_file(&db, &[("k1", "new"), ("k2", "v")])?;
    let third = write_file(&db, &[("o", "v")])?;
    let snapshot = db.snapshot();
    db.insert_stt_files(&[
        (cf.clone(), first.clone()),
        (cf.clone(), second.clone()),
        (other.clone(), third.clone()),
    ])?;

    assert!(
        snapshot.is_empty(&other)?,
        "older snapshots must not see the load"
    );
    let after = db.snapshot();
    assert_eq!(keys(after.iter(&cf))?, bytes(&["k1", "k2", "k3", "x"]));
    // The last file wins for duplicate keys
    assert_eq!(after.get(&cf, b"k1")?.as_deref(), Some(&b"new"[..]));
    // The load overrides the earlier removal
    assert_eq!(after.get(&cf, b"x")?.as_deref(), Some(&b"back"[..]));
    assert_eq!(keys(after.iter(&other))?, bytes(&["o"]));
    // The files are left for the caller to delete; the record is cleared
    assert!(first.exists() && second.exists() && third.exists());
    assert!(db.inner.meta.get(BULK_LOAD_KEY)?.is_none());
    Ok(())
}

#[test]
fn bulk_file_rejects_unsorted_keys() -> TestResult {
    let dir = TempDir::new()?;
    let db = open(dir.path())?;
    let mut file = db.new_sst_file()?;
    file.insert(b"b", b"")?;
    assert!(file.insert(b"a", b"").is_err());
    assert!(file.insert(b"b", b"").is_err());
    Ok(())
}

#[test]
fn interrupted_bulk_load_is_completed_on_open() -> TestResult {
    let dir = TempDir::new()?;
    let file = {
        let db = open(dir.path())?;
        let cf = db.column_family("a")?;
        let file = write_file(&db, &[("k", "v")])?;
        // Simulates a crash after the record is written, before ingestion
        let entries = [(&cf, vec![file.clone()])];
        db.inner.write_bulk_load_record(entries.iter())?;
        file
    };
    let db = open(dir.path())?;
    let cf = db.column_family("a")?;
    assert_eq!(db.snapshot().get(&cf, b"k")?.as_deref(), Some(&b"v"[..]));
    assert!(db.inner.meta.get(BULK_LOAD_KEY)?.is_none());
    assert!(!file.exists(), "recovered files are deleted");
    Ok(())
}

#[test]
fn read_only_is_a_point_in_time_copy() -> TestResult {
    let dir = TempDir::new()?;
    let read_write = open(dir.path())?;
    let cf = read_write.column_family("a")?;
    commit_keys(&read_write, &cf, &["before"])?;
    read_write.flush()?;

    // Opened while the read-write handle is still open
    let read_only = Db::open_read_only(dir.path(), definitions())?;
    assert!(!read_only.is_writable());
    let ro_cf = read_only.column_family("a")?;
    commit_keys(&read_write, &cf, &["after"])?;

    assert_eq!(keys(read_only.snapshot().iter(&ro_cf))?, bytes(&["before"]));
    assert!(read_only.get(&ro_cf, b"before")?.is_some());
    assert!(read_only.start_transaction().is_err());
    assert!(read_only.insert(&ro_cf, b"x", b"").is_err());
    assert!(read_only.new_sst_file().is_err());
    assert!(read_only.flush().is_err());

    let copy_path = read_only
        .inner
        .copy
        .as_ref()
        .map(|c| c.path().to_path_buf())
        .ok_or("read-only handles have a private copy")?;
    assert!(copy_path.exists());
    drop(ro_cf);
    drop(read_only);
    assert!(!copy_path.exists(), "the private copy is deleted on drop");
    Ok(())
}

#[test]
fn read_only_requires_existing_column_families() -> TestResult {
    let dir = TempDir::new()?;
    drop(open(dir.path())?);
    let mut definitions = definitions();
    definitions.push(ColumnFamilyDefinition {
        name: "new",
        use_iter: true,
        min_prefix_size: 0,
        unordered_writes: false,
    });
    assert!(matches!(
        Db::open_read_only(dir.path(), definitions),
        Err(KvError::MissingColumnFamily("new"))
    ));
    Ok(())
}

#[test]
fn backup_is_a_consistent_copy() -> TestResult {
    let dir = TempDir::new()?;
    let backup_parent = TempDir::new()?;
    let backup = backup_parent.path().join("backup");
    let db = open(dir.path())?;
    let cf = db.column_family("a")?;
    commit_keys(&db, &cf, &["k"])?;
    db.backup(&backup)?;
    commit_keys(&db, &cf, &["later"])?;
    assert!(db.backup(&backup).is_err(), "the target must not exist");

    let restored = Db::open_read_only(&backup, definitions())?;
    let restored_cf = restored.column_family("a")?;
    assert_eq!(keys(restored.snapshot().iter(&restored_cf))?, bytes(&["k"]));
    drop(restored_cf);
    drop(restored);
    // A backup is a regular database
    let restored = open(&backup)?;
    commit_keys(&restored, &restored.column_family("b")?, &["x"])?;
    Ok(())
}

#[test]
fn foreign_directories_are_refused() -> TestResult {
    let dir = TempDir::new()?;
    fs::write(dir.path().join("CURRENT"), b"MANIFEST-000001\n")?;
    assert!(open(dir.path()).is_err());
    assert!(Db::open_read_only(dir.path(), definitions()).is_err());
    // Nothing was written into it
    assert_eq!(fs::read_dir(dir.path())?.count(), 1);
    Ok(())
}

#[test]
fn missing_directory_is_created_for_read_write_only() -> TestResult {
    let parent = TempDir::new()?;
    let path = parent.path().join("new");
    assert!(Db::open_read_only(&path, definitions()).is_err());
    drop(open(&path)?);
    assert!(path.join(MARKER_FILE).exists());
    Ok(())
}

#[test]
fn large_overlapping_bulk_load() -> TestResult {
    let dir = TempDir::new()?;
    let db = open(dir.path())?;
    let cf = db.column_family("a")?;
    commit_keys(&db, &cf, &["existing"])?;
    // Even and odd keys in two files, overlapping on multiples of 1000
    let mut even = db.new_sst_file()?;
    let mut odd = db.new_sst_file()?;
    for i in 0..300_000_u32 {
        let key = i.to_be_bytes();
        if i % 2 == 0 {
            even.insert(&key, b"even")?;
        } else {
            odd.insert(&key, b"odd")?;
        }
        if i % 1000 == 0 {
            odd.insert(&[key.as_slice(), &[0]].concat(), b"")?;
            odd.insert_empty(&[key.as_slice(), &[1]].concat())?;
        }
    }
    let (even, odd) = (even.finish()?, odd.finish()?);
    db.insert_stt_files(&[(cf.clone(), even), (cf.clone(), odd)])?;
    let snapshot = db.snapshot();
    assert_eq!(snapshot.len(&cf)?, 300_000 + 2 * 300 + 1);
    assert_eq!(
        snapshot.get(&cf, &7_u32.to_be_bytes())?.as_deref(),
        Some(&b"odd"[..])
    );
    assert!(snapshot.contains_key(&cf, b"existing")?);
    Ok(())
}
