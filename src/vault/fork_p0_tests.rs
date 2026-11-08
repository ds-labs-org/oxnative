//! Fork integration, phase P0 (#25): sealed-ness accessor and bulk files for
//! non-vault column families on a vault database.

#![expect(clippy::panic_in_result_fn, reason = "assertions in tests")]

use super::*;
use crate::{ColumnFamily, ColumnFamilyDefinition, Db, DbOptions};
use std::error::Error;
use std::fs;
use std::path::Path;
use tempfile::TempDir;

type TestResult = Result<(), Box<dyn Error>>;

const SEALED: &str = "sealed";
const INDEX_A: &str = "index_a";
const INDEX_B: &str = "index_b";
const SEALED_VALUE: &[u8] = b"distinctive-sealed-value-0123456789";
const INDEX_VALUE: &[u8] = b"distinctive-index-value-abcdefghij";

fn open_vault(path: &Path) -> Result<Db, KvError> {
    let definitions = [SEALED, INDEX_A, INDEX_B]
        .into_iter()
        .map(|name| ColumnFamilyDefinition {
            name,
            use_iter: true,
            min_prefix_size: 0,
            unordered_writes: false,
        })
        .collect();
    Db::open_read_write_with_vault(
        path,
        definitions,
        DbOptions::default(),
        VaultOptions {
            key: VaultKey::new([0x33; 32]),
            vault_column_families: vec![SEALED],
        },
    )
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

fn get_vec(db: &Db, cf: &ColumnFamily, key: &[u8]) -> Result<Option<Vec<u8>>, KvError> {
    Ok(db.get(cf, key)?.map(Vec::from))
}

#[test]
fn is_sealed_is_true_only_for_vault_column_families() -> TestResult {
    let dir = TempDir::new()?;
    let db = open_vault(dir.path())?;
    let sealed = db.column_family(SEALED)?;
    let index = db.column_family(INDEX_A)?;
    assert!(sealed.is_sealed());
    assert!(!index.is_sealed());
    assert!(db.is_sealed(&sealed));
    assert!(!db.is_sealed(&index));
    Ok(())
}

#[test]
fn one_call_mixes_a_sealed_file_and_plain_files_of_non_vault_cfs() -> TestResult {
    let dir = TempDir::new()?;
    let db = open_vault(dir.path())?;
    let sealed = db.column_family(SEALED)?;
    let index_a = db.column_family(INDEX_A)?;
    let index_b = db.column_family(INDEX_B)?;

    let mut file = db.new_sst_file_for(&sealed)?;
    file.insert(b"k", SEALED_VALUE)?;
    let sealed_path = file.finish()?;
    let mut file = db.new_sst_file_for(&index_a)?;
    file.insert(b"k", INDEX_VALUE)?;
    let a_path = file.finish()?;
    let mut file = db.new_sst_file_for(&index_b)?;
    file.insert(b"k", INDEX_VALUE)?;
    let b_path = file.finish()?;

    assert!(!contains(&fs::read(&sealed_path)?, SEALED_VALUE));
    assert!(contains(&fs::read(&a_path)?, INDEX_VALUE));
    assert!(contains(&fs::read(&b_path)?, INDEX_VALUE));

    db.insert_stt_files(&[
        (index_a.clone(), a_path),
        (sealed.clone(), sealed_path),
        (index_b.clone(), b_path),
    ])?;
    assert_eq!(get_vec(&db, &sealed, b"k")?.as_deref(), Some(SEALED_VALUE));
    assert_eq!(get_vec(&db, &index_a, b"k")?.as_deref(), Some(INDEX_VALUE));
    assert_eq!(get_vec(&db, &index_b, b"k")?.as_deref(), Some(INDEX_VALUE));
    assert!(db.is_sealed(&sealed) && !db.is_sealed(&index_a) && !db.is_sealed(&index_b));
    Ok(())
}

#[test]
fn non_vault_bulk_file_ingests_and_sealed_cfs_stay_sealed() -> TestResult {
    let dir = TempDir::new()?;
    let db = open_vault(dir.path())?;
    let sealed = db.column_family(SEALED)?;
    let index = db.column_family(INDEX_A)?;

    let mut file = db.new_sst_file_for(&index)?;
    file.insert(b"a", INDEX_VALUE)?;
    let path = file.finish()?;
    db.insert_stt_files(&[(index.clone(), path)])?;
    assert_eq!(get_vec(&db, &index, b"a")?.as_deref(), Some(INDEX_VALUE));

    // The plain entry point still refuses on a vault database ...
    assert!(db.new_sst_file().is_err());
    // ... a plain file never loads into a sealed CF ...
    let mut file = db.new_sst_file_for(&index)?;
    file.insert(b"b", SEALED_VALUE)?;
    let plain_path = file.finish()?;
    assert!(
        db.insert_stt_files(&[(sealed.clone(), plain_path)])
            .is_err()
    );
    assert_eq!(get_vec(&db, &sealed, b"b")?, None);
    // ... and a file made for a sealed CF is sealed.
    let mut file = db.new_sst_file_for(&sealed)?;
    file.insert(b"c", SEALED_VALUE)?;
    assert!(!contains(&fs::read(file.finish()?)?, SEALED_VALUE));
    assert!(db.is_sealed(&sealed));
    Ok(())
}
