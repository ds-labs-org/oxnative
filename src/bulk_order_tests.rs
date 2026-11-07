//! Bulk-load files must be strictly ordered; a tampered file is refused
//! cleanly (#15).

#![expect(clippy::panic_in_result_fn, reason = "assertions in tests")]

use super::*;
use std::error::Error;
use std::fs;
use std::panic::{AssertUnwindSafe, catch_unwind};
use tempfile::TempDir;

type TestResult = Result<(), Box<dyn Error>>;

const CF: &str = "cf";
const VALUES: [(&[u8], &[u8]); 3] = [(b"a", b"value-1"), (b"b", b"value-2"), (b"c", b"value-3")];

fn definitions() -> Vec<ColumnFamilyDefinition> {
    [CF, "other"]
        .into_iter()
        .map(|name| ColumnFamilyDefinition {
            name,
            use_iter: true,
            min_prefix_size: 0,
            unordered_writes: false,
        })
        .collect()
}

fn open_plain(path: &Path) -> Result<Db, KvError> {
    Db::open_read_write(path, definitions(), DbOptions::default())
}

#[cfg(feature = "vault")]
fn open_vault(path: &Path) -> Result<Db, KvError> {
    Db::open_read_write_with_vault(
        path,
        definitions(),
        DbOptions::default(),
        VaultOptions {
            key: VaultKey::new([0x11; 32]),
            vault_column_families: vec![CF],
        },
    )
}

/// Byte ranges of the records of a bulk-load file, and the header length.
fn record_ranges(
    bytes: &[u8],
    sealed: bool,
) -> Result<(usize, Vec<(usize, usize)>), Box<dyn Error>> {
    let word = |at: usize| -> Result<usize, Box<dyn Error>> {
        let raw: [u8; 4] = bytes.get(at..at + 4).ok_or("cut file")?.try_into()?;
        Ok(usize::try_from(u32::from_le_bytes(raw))?)
    };
    let mut at = 8;
    if sealed {
        at += 4 + word(at)?;
    }
    let header = at;
    let mut ranges = Vec::new();
    while at < bytes.len() {
        let start = at;
        at += 4 + word(at)?;
        at += 4 + word(at)?;
        ranges.push((start, at));
    }
    Ok((header, ranges))
}

/// Rewrites the file with its records permuted: `order` lists the indexes of
/// the original records in their new position.
fn permute(path: &Path, sealed: bool, order: &[usize]) -> TestResult {
    let bytes = fs::read(path)?;
    let (header, ranges) = record_ranges(&bytes, sealed)?;
    let mut out = bytes.get(..header).ok_or("cut file")?.to_vec();
    for i in order {
        let (start, end) = *ranges.get(*i).ok_or("no such record")?;
        out.extend_from_slice(bytes.get(start..end).ok_or("cut file")?);
    }
    fs::write(path, out)?;
    Ok(())
}

fn stored_record(db: &Db) -> Result<Option<Vec<u8>>, KvError> {
    Ok(db.inner.meta.get(BULK_LOAD_KEY)?.map(|v| v.to_vec()))
}

/// A load of a tampered file must return an error (never panic), leave no
/// persisted record, keep the locks usable and the database reopenable.
fn assert_clean_refusal(
    dir: &TempDir,
    open: &dyn Fn(&Path) -> Result<Db, KvError>,
    sealed: bool,
    order: &[usize],
    what: &str,
) -> TestResult {
    let db = open(dir.path())?;
    let cf = db.column_family(CF)?;
    let mut file = db.new_sst_file_for(&cf)?;
    for (key, value) in VALUES {
        file.insert(key, value)?;
    }
    let path = file.finish()?;
    permute(&path, sealed, order)?;

    let result = catch_unwind(AssertUnwindSafe(|| {
        db.insert_stt_files(&[(cf.clone(), path.clone())])
    }));
    let Ok(result) = result else {
        return Err(format!("{what}: insert_stt_files panicked").into());
    };
    assert!(result.is_err(), "{what}: the file must be refused");
    assert_eq!(
        stored_record(&db)?,
        None,
        "{what}: no bulk_load record may stay persisted"
    );
    for (key, _) in VALUES {
        assert!(db.get(&cf, key)?.is_none(), "{what}: nothing may be loaded");
    }
    // Locks are not poisoned: writes and snapshots still work
    let mut transaction = db.start_transaction()?;
    transaction.insert(&cf, b"z", b"after");
    transaction.commit()?;
    assert_eq!(
        db.snapshot().get(&cf, b"z")?.as_deref(),
        Some(&b"after"[..])
    );
    drop((db, cf));
    let reopened = open(dir.path());
    assert!(
        reopened.is_ok(),
        "{what}: the next open_read_write must succeed"
    );
    Ok(())
}

#[test]
fn plain_file_with_swapped_records_is_refused_cleanly() -> TestResult {
    assert_clean_refusal(
        &TempDir::new()?,
        &open_plain,
        false,
        &[1, 0, 2],
        "plain swapped",
    )
}

#[test]
fn plain_file_with_duplicate_keys_is_refused_cleanly() -> TestResult {
    assert_clean_refusal(
        &TempDir::new()?,
        &open_plain,
        false,
        &[0, 1, 1, 2],
        "plain duplicate",
    )
}

#[test]
fn plain_file_in_reverse_order_is_refused_cleanly() -> TestResult {
    assert_clean_refusal(
        &TempDir::new()?,
        &open_plain,
        false,
        &[2, 1, 0],
        "plain reversed",
    )
}

#[cfg(feature = "vault")]
#[test]
fn sealed_file_with_swapped_records_is_refused_cleanly() -> TestResult {
    assert_clean_refusal(
        &TempDir::new()?,
        &open_vault,
        true,
        &[1, 0, 2],
        "sealed swapped",
    )
}

#[cfg(feature = "vault")]
#[test]
fn sealed_file_with_duplicate_keys_is_refused_cleanly() -> TestResult {
    assert_clean_refusal(
        &TempDir::new()?,
        &open_vault,
        true,
        &[0, 1, 1, 2],
        "sealed duplicate",
    )
}

/// `verify` itself, and the reader behind `ingest`, report `Corruption`.
#[test]
fn verify_and_ingest_report_corruption_on_disorder() -> TestResult {
    let dir = TempDir::new()?;
    let db = open_plain(dir.path())?;
    let cf = db.column_family(CF)?;
    let mut file = db.new_sst_file_for(&cf)?;
    for (key, value) in VALUES {
        file.insert(key, value)?;
    }
    let path = file.finish()?;
    permute(&path, false, &[1, 0, 2])?;
    let paths = [path];
    let verified = sst::verify(&paths, None);
    assert!(
        matches!(verified, Err(KvError::Corruption(_))),
        "verify must flag disorder as corruption: {verified:?}"
    );
    let ingested = catch_unwind(AssertUnwindSafe(|| sst::ingest(&cf.keyspace, &paths, None)));
    assert!(
        matches!(ingested, Ok(Err(KvError::Corruption(_)))),
        "Reader::next must return Corruption on disorder, never panic"
    );
    Ok(())
}

/// Recovery of a recorded load whose file is bad must not panic: it either
/// fails with an error or discards the load, and then the database opens.
fn assert_recovery_does_not_panic(
    dir: &TempDir,
    open: &dyn Fn(&Path) -> Result<Db, KvError>,
    sealed: bool,
) -> TestResult {
    {
        let db = open(dir.path())?;
        let cf = db.column_family(CF)?;
        let mut file = db.new_sst_file_for(&cf)?;
        for (key, value) in VALUES {
            file.insert(key, value)?;
        }
        let path = file.finish()?;
        permute(&path, sealed, &[1, 0, 2])?;
        // As left by a crash after the record was written
        let files = [(&cf, vec![path])];
        db.inner.write_bulk_load_record(files.iter())?;
    }
    let first = catch_unwind(AssertUnwindSafe(|| open(dir.path()).map(|_| ())));
    assert!(first.is_ok(), "recover_bulk_load panicked on a bad file");
    if first.is_ok_and(|r| r.is_err()) {
        // Failing cleanly is acceptable, but it must not wedge the next open
        // with a panic either
        let second = catch_unwind(AssertUnwindSafe(|| open(dir.path()).map(|_| ())));
        assert!(second.is_ok(), "a later open panicked");
    }
    Ok(())
}

#[test]
fn recovery_of_a_plain_bulk_load_with_a_bad_file_does_not_panic() -> TestResult {
    assert_recovery_does_not_panic(&TempDir::new()?, &open_plain, false)
}

#[cfg(feature = "vault")]
#[test]
fn recovery_of_a_sealed_bulk_load_with_a_bad_file_does_not_panic() -> TestResult {
    assert_recovery_does_not_panic(&TempDir::new()?, &open_vault, true)
}
