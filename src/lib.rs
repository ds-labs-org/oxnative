#![cfg_attr(doc, doc = include_str!("../README.md"))]

mod copy;
mod error;
mod overlay;
mod sst;
#[cfg(feature = "vault")]
mod vault;

use crate::copy::PrivateCopy;
pub use crate::error::KvError;
use crate::overlay::{Cursor, Overlay};
#[cfg(feature = "vault")]
pub use crate::vault::{VaultKey, VaultOptions};
use fjall::config::CompressionPolicy;
use fjall::{CompressionType, KeyspaceCreateOptions, PersistMode, Readable, Slice};
use std::borrow::{Borrow, Cow};
use std::collections::BTreeMap;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::{self, Write as _};
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock, RwLockReadGuard};
use std::time::{SystemTime, UNIX_EPOCH};

/// File identifying an oxnative database directory.
const MARKER_FILE: &str = "OXNATIVE";
const MARKER_CONTENT: &str = "oxnative 1\nengine fjall 3\n";
/// Marker line added to databases whose vault column families are encrypted.
const MARKER_VAULT_LINE: &str = "vault 1";
/// Prefix of every marker line that describes the vault format.
const MARKER_VAULT_PREFIX: &str = "vault ";
/// Subdirectory holding the fjall database.
const FJALL_DIR: &str = "fjall";
/// Subdirectory holding bulk-load files until they are ingested.
const BULK_DIR: &str = "bulk";
/// Internal keyspace, never exposed as a column family.
const META_KEYSPACE: &str = "_oxnative_meta";
/// Column family that always exists, as in RocksDB.
const DEFAULT_COLUMN_FAMILY: &str = "default";
/// Key of the in-progress bulk load record in [`META_KEYSPACE`].
const BULK_LOAD_KEY: &[u8] = b"bulk_load";
/// Key of the vault key check in [`META_KEYSPACE`]. Only vaulted databases
/// have it.
const VAULT_CHECK_KEY: &[u8] = b"vault_check";

/// Without the `vault` feature no database can be opened as vaulted.
#[cfg(not(feature = "vault"))]
enum VaultOptions {}

/// Definition of a column family, given when opening the database.
///
/// The set of column families is fixed for the lifetime of a [`Db`]. As in
/// RocksDB, a `default` column family always exists, even if not listed.
pub struct ColumnFamilyDefinition {
    /// Name of the column family.
    pub name: &'static str,
    /// Whether the column family is iterated over. Not used by this engine.
    pub use_iter: bool,
    /// Length of the key prefix used by prefix scans. Not used by this engine.
    pub min_prefix_size: usize,
    /// Whether writes to this column family may be reordered. Not used by this
    /// engine.
    pub unordered_writes: bool,
}

/// Options for [`Db::open_read_write`].
#[derive(Clone, Debug, Default)]
#[cfg_attr(not(feature = "vault"), derive(Copy))]
pub struct DbOptions {
    /// Maximum number of table files kept open.
    pub max_open_files: Option<i32>,
    /// Number of file descriptors to leave to the rest of the process. Not
    /// used by this engine.
    pub fd_reserve: Option<u32>,
    /// Encryption of the vault column families. `None` keeps the database
    /// plain.
    #[cfg(feature = "vault")]
    pub vault: Option<VaultOptions>,
}

/// A database handle. Cheap to clone, `Send + Sync`.
#[derive(Clone)]
pub struct Db {
    inner: Arc<DbInner>,
}

struct DbInner {
    db: fjall::Database,
    meta: fjall::Keyspace,
    column_families: Vec<(&'static str, ColumnFamily)>,
    root: PathBuf,
    writable: bool,
    /// Serializes writes, so that `remove_range` and bulk loads see a stable
    /// committed state.
    commit_lock: Mutex<()>,
    /// Taken for reading to create snapshots and read the latest state, and for
    /// writing by bulk loads, so that a reader sees all or none of the column
    /// families of a bulk load.
    visibility: RwLock<()>,
    /// Private copy opened by read-only handles; dropped after `db`.
    #[cfg_attr(not(test), expect(dead_code, reason = "only kept for its Drop"))]
    copy: Option<PrivateCopy>,
}

impl Db {
    /// Opens or creates the database at `path` in read-write mode.
    ///
    /// Column families listed in `column_families` that do not exist yet are
    /// created. Only one read-write handle may be open on a directory at a time.
    #[expect(
        clippy::needless_pass_by_value,
        reason = "same signature as Oxigraph's RocksDB wrapper"
    )]
    pub fn open_read_write(
        path: &Path,
        column_families: Vec<ColumnFamilyDefinition>,
        db_options: DbOptions,
    ) -> Result<Self, KvError> {
        let vault = db_options_vault(&db_options);
        let created = prepare_root(path, vault.is_some())?;
        let mut builder = fjall::Database::builder(path.join(FJALL_DIR));
        if let Some(max_open_files) = db_options
            .max_open_files
            .and_then(|n| usize::try_from(n).ok())
            .filter(|n| *n > 0)
        {
            builder = builder.max_cached_files(Some(max_open_files));
        }
        let db = builder.open()?;
        verify_vault_state(&db, vault, created)?;
        let inner = DbInner::new(db, &column_families, vault, path, true, None)?;
        inner.recover_bulk_load()?;
        Ok(Self {
            inner: Arc::new(inner),
        })
    }

    /// Opens an existing database at `path` in read-only mode.
    ///
    /// The handle sees the database as it was when opened: it reads a private
    /// copy (table files are hard-linked when possible), so it can coexist with
    /// a read-write handle on the same directory. Opening while another
    /// process writes to the database is not supported.
    #[expect(
        clippy::needless_pass_by_value,
        reason = "same signature as Oxigraph's RocksDB wrapper"
    )]
    pub fn open_read_only(
        path: &Path,
        column_families: Vec<ColumnFamilyDefinition>,
    ) -> Result<Self, KvError> {
        Self::open_read_only_inner(path, &column_families, None)
    }

    /// Opens an existing vaulted database at `path` in read-only mode. The key
    /// in `vault` is required and checked, as for [`Db::open_read_write`].
    #[cfg(feature = "vault")]
    #[expect(
        clippy::needless_pass_by_value,
        reason = "same shape as the other open functions"
    )]
    pub fn open_read_only_with_vault(
        path: &Path,
        column_families: Vec<ColumnFamilyDefinition>,
        vault: VaultOptions,
    ) -> Result<Self, KvError> {
        Self::open_read_only_inner(path, &column_families, Some(&vault))
    }

    fn open_read_only_inner(
        path: &Path,
        column_families: &[ColumnFamilyDefinition],
        vault: Option<&VaultOptions>,
    ) -> Result<Self, KvError> {
        check_marker(path, vault.is_some())?;
        let source = path.join(FJALL_DIR);
        if !source.is_dir() {
            return Err(KvError::corruption(format!(
                "{} has no {FJALL_DIR} directory",
                path.display()
            )));
        }
        let copy = PrivateCopy::create(&source, path)?;
        let db = fjall::Database::builder(copy.path()).open()?;
        verify_vault_state(&db, vault, false)?;
        let inner = DbInner::new(db, column_families, vault, path, false, Some(copy))?;
        Ok(Self {
            inner: Arc::new(inner),
        })
    }

    /// Whether this handle was opened with [`Db::open_read_write`].
    pub fn is_writable(&self) -> bool {
        self.inner.writable
    }

    /// Returns the handle of a column family given at open time.
    pub fn column_family(&self, name: &'static str) -> Result<ColumnFamily, KvError> {
        self.inner
            .column_families
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, cf)| cf.clone())
            .ok_or(KvError::MissingColumnFamily(name))
    }

    /// Returns a point-in-time view of the database.
    #[must_use]
    pub fn snapshot(&self) -> Reader<'static> {
        Reader {
            kind: ReaderKind::Snapshot {
                snapshot: self.inner.snapshot(),
                db: Arc::clone(&self.inner),
            },
        }
    }

    /// Starts a write-only batch, applied atomically by [`Transaction::commit`].
    pub fn start_transaction(&self) -> Result<Transaction, KvError> {
        self.inner.check_writable("Transaction")?;
        Ok(Transaction {
            db: Arc::clone(&self.inner),
            overlay: Overlay::default(),
        })
    }

    /// Starts a transaction that reads a snapshot taken now plus its own
    /// pending writes, applied atomically by [`ReadableTransaction::commit`].
    pub fn start_readable_transaction(&self) -> Result<ReadableTransaction<'_>, KvError> {
        self.inner.check_writable("Transaction")?;
        Ok(ReadableTransaction {
            db: &self.inner,
            snapshot: self.inner.snapshot(),
            overlay: Overlay::default(),
        })
    }

    /// Reads the latest committed value of `key`.
    pub fn get(
        &self,
        column_family: &ColumnFamily,
        key: &[u8],
    ) -> Result<Option<PinnableSlice>, KvError> {
        let _visibility = self.inner.read_visibility();
        column_family
            .keyspace
            .get(key)?
            .map(|value| column_family.unseal(key, value))
            .transpose()
    }

    /// Whether `key` exists in the latest committed state.
    pub fn contains_key(&self, column_family: &ColumnFamily, key: &[u8]) -> Result<bool, KvError> {
        let _visibility = self.inner.read_visibility();
        Ok(column_family.keyspace.contains_key(key)?)
    }

    /// Writes a single key outside of any batch.
    pub fn insert(
        &self,
        column_family: &ColumnFamily,
        key: &[u8],
        value: &[u8],
    ) -> Result<(), KvError> {
        self.inner.check_writable("Insert")?;
        let _commit = self.inner.lock_commit()?;
        Ok(column_family
            .keyspace
            .insert(key, column_family.seal(key, value)?.as_ref())?)
    }

    /// Persists all committed writes to disk.
    pub fn flush(&self) -> Result<(), KvError> {
        self.inner.check_writable("Flush")?;
        Ok(self.inner.db.persist(PersistMode::SyncAll)?)
    }

    /// Compacts a column family into as few tables as possible.
    pub fn compact(&self, column_family: &ColumnFamily) -> Result<(), KvError> {
        self.inner.check_writable("Compaction")?;
        Ok(column_family.keyspace.major_compact()?)
    }

    /// Creates a writer for a sorted file, written outside the database and
    /// added later with [`Db::insert_stt_files`]. Keys are inserted in strictly
    /// increasing order.
    pub fn new_sst_file(&self) -> Result<SstFileWriter, KvError> {
        self.inner.check_writable("Bulk-load file creation")?;
        let path = self
            .inner
            .root
            .join(BULK_DIR)
            .join(format!("{}.sst", unique_suffix()));
        Ok(SstFileWriter(sst::Writer::create(path)?))
    }

    /// Atomically adds files produced by [`SstFileWriter::finish`] to the
    /// given column families. An empty list is a no-op. The files are left in
    /// place; the caller deletes them.
    ///
    /// Readers see either none or all of the files. If the process stops in
    /// the middle, the load is completed when the database is next opened in
    /// read-write mode.
    pub fn insert_stt_files(&self, ssts_for_cf: &[(ColumnFamily, PathBuf)]) -> Result<(), KvError> {
        self.inner.check_writable("Bulk-load file ingestion")?;
        if ssts_for_cf.is_empty() {
            return Ok(());
        }
        let mut by_cf = BTreeMap::<usize, (&ColumnFamily, Vec<PathBuf>)>::new();
        for (cf, path) in ssts_for_cf {
            by_cf
                .entry(cf.index)
                .or_insert_with(|| (cf, Vec::new()))
                .1
                .push(path.clone());
        }
        let _commit = self.inner.lock_commit()?;
        self.inner.write_bulk_load_record(by_cf.values())?;
        {
            let _visibility = self
                .inner
                .visibility
                .write()
                .map_err(|_| KvError::poisoned())?;
            for (cf, paths) in by_cf.values() {
                sst::ingest(&cf.keyspace, paths)?;
            }
        }
        self.inner.clear_bulk_load_record()
    }

    /// Writes a consistent copy of the database into `target_directory`, which
    /// must not exist yet. The copy can be opened with [`Db::open_read_write`]
    /// and [`Db::open_read_only`].
    ///
    /// The copy is logical (every key is rewritten), so it takes time and space
    /// proportional to the database size.
    pub fn backup(&self, target_directory: &Path) -> Result<(), KvError> {
        #[expect(clippy::create_dir, reason = "the target must not exist yet")]
        fs::create_dir(target_directory)?;
        let result = self.inner.backup_into(target_directory);
        if result.is_err() {
            // Best effort: do not leave a half-written backup behind
            #[expect(unused_must_use, reason = "the backup error is the one to report")]
            fs::remove_dir_all(target_directory);
        }
        result
    }
}

impl DbInner {
    #[cfg_attr(
        not(feature = "vault"),
        expect(unused_variables, reason = "only vaulted databases need it")
    )]
    fn new(
        db: fjall::Database,
        definitions: &[ColumnFamilyDefinition],
        vault: Option<&VaultOptions>,
        root: &Path,
        writable: bool,
        copy: Option<PrivateCopy>,
    ) -> Result<Self, KvError> {
        // Like RocksDB, there is always a "default" column family
        let mut names = definitions.iter().map(|d| d.name).collect::<Vec<_>>();
        if !names.contains(&DEFAULT_COLUMN_FAMILY) {
            names.push(DEFAULT_COLUMN_FAMILY);
        }
        #[cfg(feature = "vault")]
        let vault_key = vault.map(|v| Arc::new(v.key.clone()));
        #[cfg(feature = "vault")]
        if let Some(vault) = vault
            && let Some(unknown) = vault
                .vault_column_families
                .iter()
                .find(|name| !names.contains(name))
        {
            return Err(KvError::other(format!(
                "vault column family {unknown:?} is not a column family of the database"
            )));
        }
        let mut column_families = Vec::with_capacity(names.len());
        for (index, name) in names.into_iter().enumerate() {
            if name.is_empty() || name.len() > usize::from(u8::MAX) || name == META_KEYSPACE {
                return Err(KvError::other(format!(
                    "invalid column family name {name:?}"
                )));
            }
            if !writable && !db.keyspace_exists(name) {
                return Err(KvError::MissingColumnFamily(name));
            }
            let keyspace = db.keyspace(name, keyspace_options)?;
            column_families.push((
                name,
                ColumnFamily {
                    index,
                    keyspace,
                    #[cfg(feature = "vault")]
                    seal: vault
                        .zip(vault_key.as_ref())
                        .filter(|(v, _)| v.vault_column_families.contains(&name))
                        .map(|(_, key)| vault::Seal::new(Arc::clone(key), name)),
                },
            ));
        }
        let meta = db.keyspace(META_KEYSPACE, KeyspaceCreateOptions::default)?;
        Ok(Self {
            db,
            meta,
            column_families,
            root: root.to_path_buf(),
            writable,
            commit_lock: Mutex::new(()),
            visibility: RwLock::new(()),
            copy,
        })
    }

    fn check_writable(&self, operation: &str) -> Result<(), KvError> {
        if self.writable {
            Ok(())
        } else {
            Err(KvError::read_only(operation))
        }
    }

    fn lock_commit(&self) -> Result<MutexGuard<'_, ()>, KvError> {
        self.commit_lock.lock().map_err(|_| KvError::poisoned())
    }

    fn read_visibility(&self) -> RwLockReadGuard<'_, ()> {
        // The guarded value is `()`: a poisoned lock protects nothing broken
        self.visibility
            .read()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn snapshot(&self) -> fjall::Snapshot {
        let _visibility = self.read_visibility();
        self.db.snapshot()
    }

    fn column_family_at(&self, index: usize) -> Result<&ColumnFamily, KvError> {
        self.column_families
            .get(index)
            .map(|(_, cf)| cf)
            .ok_or_else(|| KvError::other("column family from another database"))
    }

    fn column_family_named(&self, name: &str) -> Result<&ColumnFamily, KvError> {
        self.column_families
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, cf)| cf)
            .ok_or_else(|| {
                KvError::corruption(format!("unknown column family {name} in bulk-load record"))
            })
    }

    fn commit(&self, overlay: &Overlay) -> Result<(), KvError> {
        let _commit = self.lock_commit()?;
        let mut batch = self.db.batch();
        for &index in overlay.cleared.keys() {
            let cf = self.column_family_at(index)?;
            for key in overlay.keys_to_clear(index, &cf.keyspace)? {
                batch.remove(&cf.keyspace, key);
            }
        }
        for (&index, writes) in &overlay.writes {
            let cf = self.column_family_at(index)?;
            for (key, value) in writes {
                match value {
                    Some(value) => {
                        batch.insert(&cf.keyspace, key.as_slice(), cf.seal(key, value)?.as_ref())
                    }
                    None => batch.remove(&cf.keyspace, key.as_slice()),
                }
            }
        }
        Ok(batch.commit()?)
    }

    /// Records the files of a bulk load durably before ingesting them, so that
    /// an interrupted load can be completed on the next open.
    fn write_bulk_load_record<'a>(
        &self,
        files: impl Iterator<Item = &'a (&'a ColumnFamily, Vec<PathBuf>)>,
    ) -> Result<(), KvError> {
        let mut record = String::new();
        for (cf, paths) in files {
            let name = self
                .column_families
                .get(cf.index)
                .map(|(n, _)| *n)
                .ok_or_else(|| KvError::other("column family from another database"))?;
            for path in paths {
                let path = path.to_str().ok_or_else(|| {
                    KvError::other(format!(
                        "bulk-load file path {} is not valid UTF-8",
                        path.display()
                    ))
                })?;
                if path.contains(['\t', '\n']) {
                    return Err(KvError::other(format!(
                        "bulk-load file path {path:?} contains a tab or a newline"
                    )));
                }
                record.push_str(name);
                record.push('\t');
                record.push_str(path);
                record.push('\n');
            }
        }
        self.meta.insert(BULK_LOAD_KEY, record.as_bytes())?;
        self.db.persist(PersistMode::SyncAll)?;
        Ok(())
    }

    fn clear_bulk_load_record(&self) -> Result<(), KvError> {
        self.meta.remove(BULK_LOAD_KEY)?;
        // Durable before the caller deletes the files the record points to
        self.db.persist(PersistMode::SyncAll)?;
        Ok(())
    }

    /// Completes a bulk load interrupted by a crash.
    fn recover_bulk_load(&self) -> Result<(), KvError> {
        let Some(record) = self.meta.get(BULK_LOAD_KEY)? else {
            return Ok(());
        };
        let record = std::str::from_utf8(&record)
            .map_err(|_| KvError::corruption("bulk-load record is not valid UTF-8"))?;
        let mut by_cf = BTreeMap::<usize, (&ColumnFamily, Vec<PathBuf>)>::new();
        for line in record.lines() {
            let (name, path) = line
                .split_once('\t')
                .ok_or_else(|| KvError::corruption("invalid bulk-load record"))?;
            let cf = self.column_family_named(name)?;
            by_cf
                .entry(cf.index)
                .or_insert_with(|| (cf, Vec::new()))
                .1
                .push(PathBuf::from(path));
        }
        // Re-ingesting a column family that was already ingested is harmless:
        // the files hold the same keys and values.
        for (cf, paths) in by_cf.values() {
            sst::ingest(&cf.keyspace, paths)?;
        }
        self.clear_bulk_load_record()?;
        for (_, paths) in by_cf.values() {
            for path in paths {
                // The loader that created them is gone
                #[expect(unused_must_use, reason = "a leftover file only wastes space")]
                fs::remove_file(path);
            }
        }
        Ok(())
    }

    fn backup_into(&self, target: &Path) -> Result<(), KvError> {
        write_marker(target, false)?;
        #[expect(clippy::create_dir, reason = "the target was just created")]
        fs::create_dir(target.join(BULK_DIR))?;
        let target_db = fjall::Database::builder(target.join(FJALL_DIR)).open()?;
        let snapshot = self.snapshot();
        for (name, cf) in &self.column_families {
            let keyspace = target_db.keyspace(name, keyspace_options)?;
            let mut ingestion = keyspace.start_ingestion()?;
            for guard in snapshot.iter(&cf.keyspace) {
                let (key, value) = guard.into_inner()?;
                ingestion.write(key, value)?;
            }
            ingestion.finish()?;
        }
        target_db.persist(PersistMode::SyncAll)?;
        Ok(())
    }
}

/// Options of the keyspaces backing column families. fjall stores them when a
/// keyspace is created, so they only apply to new databases.
///
/// fjall compresses data blocks only from level 2 down, but bulk loads land at
/// level 1 and stay there until compacted: with that default a freshly loaded
/// database is about twice as large. Like Oxigraph's RocksDB settings, all
/// levels use LZ4 (no measurable read or write slowdown in our measurements).
fn keyspace_options() -> KeyspaceCreateOptions {
    KeyspaceCreateOptions::default()
        .data_block_compression_policy(CompressionPolicy::all(CompressionType::Lz4))
}

/// Creates the database directory, or checks that an existing one belongs to
/// this engine. Returns whether the database is new.
fn prepare_root(path: &Path, vault: bool) -> Result<bool, KvError> {
    let created = if path.join(MARKER_FILE).exists() {
        check_marker(path, vault)?;
        false
    } else if !path.exists() {
        fs::create_dir_all(path)?;
        write_marker(path, vault)?;
        true
    } else if fs::read_dir(path)?.next().is_none() {
        write_marker(path, vault)?;
        true
    } else {
        return Err(KvError::corruption(format!(
            "{} is not an oxnative database (no {MARKER_FILE} file). Databases written by \
             the RocksDB backend must be migrated with a dump and a load.",
            path.display()
        )));
    };
    fs::create_dir_all(path.join(BULK_DIR))?;
    Ok(created)
}

/// Checks the marker file, including that the vault flag matches how the
/// database is being opened.
fn check_marker(path: &Path, vault: bool) -> Result<(), KvError> {
    let content = fs::read_to_string(path.join(MARKER_FILE)).map_err(|e| {
        if e.kind() == io::ErrorKind::NotFound {
            KvError::corruption(format!(
                "{} is not an oxnative database (no {MARKER_FILE} file)",
                path.display()
            ))
        } else {
            e.into()
        }
    })?;
    if content.lines().next() != MARKER_CONTENT.lines().next() {
        return Err(KvError::corruption(format!(
            "unsupported oxnative database format in {}: {:?}",
            path.display(),
            content.lines().next().unwrap_or_default()
        )));
    }
    let mut marked_vault = false;
    for line in content
        .lines()
        .filter(|l| l.starts_with(MARKER_VAULT_PREFIX))
    {
        if line == MARKER_VAULT_LINE {
            marked_vault = true;
        } else {
            return Err(KvError::corruption(format!(
                "unsupported vault format in {}: {line:?}",
                path.display()
            )));
        }
    }
    match (marked_vault, vault) {
        (true, false) => Err(KvError::other(format!(
            "{} is a vaulted database: open it with a vault key (feature `vault`)",
            path.display()
        ))),
        (false, true) => Err(KvError::other(format!(
            "{} is not a vaulted database: a plain database is never converted to a vault \
             one, as its plaintext values would stay on disk",
            path.display()
        ))),
        _ => Ok(()),
    }
}

fn write_marker(path: &Path, vault: bool) -> Result<(), KvError> {
    let mut file = fs::File::create_new(path.join(MARKER_FILE))?;
    file.write_all(MARKER_CONTENT.as_bytes())?;
    if vault {
        writeln!(file, "{MARKER_VAULT_LINE}")?;
    }
    file.sync_all()?;
    Ok(())
}

#[cfg(feature = "vault")]
fn db_options_vault(options: &DbOptions) -> Option<&VaultOptions> {
    options.vault.as_ref()
}

#[cfg(not(feature = "vault"))]
fn db_options_vault(_options: &DbOptions) -> Option<&VaultOptions> {
    None
}

/// Fails closed unless the stored key check agrees with how the database is
/// opened. A new vaulted database gets its key check here, before any data.
#[cfg_attr(
    not(feature = "vault"),
    expect(unused_variables, reason = "only new vaulted databases need it")
)]
fn verify_vault_state(
    db: &fjall::Database,
    vault: Option<&VaultOptions>,
    created: bool,
) -> Result<(), KvError> {
    let meta = db.keyspace(META_KEYSPACE, KeyspaceCreateOptions::default)?;
    let stored = meta.get(VAULT_CHECK_KEY)?;
    let Some(vault) = vault else {
        return if stored.is_some() {
            Err(KvError::corruption(
                "this database has a vault key check but is opened without a vault key",
            ))
        } else {
            Ok(())
        };
    };
    #[cfg(feature = "vault")]
    {
        match stored {
            Some(stored) => vault::verify_check(&vault.key, &stored),
            None if created || is_empty_database(db)? => {
                meta.insert(VAULT_CHECK_KEY, vault::new_check(&vault.key)?)?;
                Ok(db.persist(PersistMode::SyncAll)?)
            }
            None => Err(KvError::corruption("the vault key check is missing")),
        }
    }
    #[cfg(not(feature = "vault"))]
    {
        match *vault {}
    }
}

/// Whether no keyspace holds any data. A vaulted database whose creation was
/// interrupted before its key check was written looks like this.
#[cfg(feature = "vault")]
fn is_empty_database(db: &fjall::Database) -> Result<bool, KvError> {
    for name in db.list_keyspace_names() {
        let keyspace = db.keyspace(&name, KeyspaceCreateOptions::default)?;
        if !keyspace.is_empty()? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn unique_suffix() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!(
        "{}-{nanos}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// Handle to a column family. Cheap to clone, `Send + Sync`, usable as a map key.
#[derive(Clone)]
pub struct ColumnFamily {
    index: usize,
    keyspace: fjall::Keyspace,
    /// Present for vault column families: encrypts their values.
    #[cfg(feature = "vault")]
    seal: Option<vault::Seal>,
}

impl ColumnFamily {
    /// The bytes stored for `value`: encrypted in a vault column family.
    #[cfg(feature = "vault")]
    fn seal<'v>(&self, key: &[u8], value: &'v [u8]) -> Result<Cow<'v, [u8]>, KvError> {
        match &self.seal {
            Some(seal) => seal.seal(key, value).map(Cow::Owned),
            None => Ok(Cow::Borrowed(value)),
        }
    }

    #[cfg(not(feature = "vault"))]
    #[expect(clippy::unnecessary_wraps, reason = "same signature as with `vault`")]
    #[expect(clippy::unused_self, reason = "same signature as with `vault`")]
    fn seal<'v>(&self, _key: &[u8], value: &'v [u8]) -> Result<Cow<'v, [u8]>, KvError> {
        Ok(Cow::Borrowed(value))
    }

    /// The value read from the database: decrypted in a vault column family.
    #[cfg(feature = "vault")]
    fn unseal(&self, key: &[u8], stored: Slice) -> Result<PinnableSlice, KvError> {
        match &self.seal {
            Some(seal) => seal
                .open(key, &stored)
                .map(|v| PinnableSlice(Slice::from(v))),
            None => Ok(PinnableSlice(stored)),
        }
    }

    #[cfg(not(feature = "vault"))]
    #[expect(clippy::unnecessary_wraps, reason = "same signature as with `vault`")]
    #[expect(clippy::unused_self, reason = "same signature as with `vault`")]
    fn unseal(&self, _key: &[u8], stored: Slice) -> Result<PinnableSlice, KvError> {
        Ok(PinnableSlice(stored))
    }
}

impl PartialEq for ColumnFamily {
    fn eq(&self, other: &Self) -> bool {
        self.index == other.index && self.keyspace == other.keyspace
    }
}

impl Eq for ColumnFamily {}

impl Hash for ColumnFamily {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.index.hash(state);
        self.keyspace.hash(state);
    }
}

/// A read view: a snapshot ([`Db::snapshot`]) or a transaction's own view
/// ([`ReadableTransaction::reader`]). Cheap to clone, `Send + Sync`.
#[derive(Clone)]
pub struct Reader<'a> {
    kind: ReaderKind<'a>,
}

#[derive(Clone)]
enum ReaderKind<'a> {
    Snapshot {
        snapshot: fjall::Snapshot,
        /// Keeps a read-only handle's private copy alive.
        db: Arc<DbInner>,
    },
    Transaction {
        snapshot: &'a fjall::Snapshot,
        overlay: &'a Overlay,
    },
}

impl<'a> Reader<'a> {
    /// Reads `key` in this view.
    pub fn get(
        &self,
        column_family: &ColumnFamily,
        key: &[u8],
    ) -> Result<Option<PinnableSlice>, KvError> {
        match &self.kind {
            ReaderKind::Snapshot { snapshot, .. } => snapshot
                .get(&column_family.keyspace, key)?
                .map(|value| column_family.unseal(key, value))
                .transpose(),
            ReaderKind::Transaction { snapshot, overlay } => {
                match overlay.get(column_family.index, key) {
                    Some(Some(value)) => Ok(Some(PinnableSlice(Slice::from(value.as_slice())))),
                    Some(None) => Ok(None),
                    None => snapshot
                        .get(&column_family.keyspace, key)?
                        .map(|value| column_family.unseal(key, value))
                        .transpose(),
                }
            }
        }
    }

    /// Whether `key` exists in this view.
    pub fn contains_key(&self, column_family: &ColumnFamily, key: &[u8]) -> Result<bool, KvError> {
        match &self.kind {
            ReaderKind::Snapshot { snapshot, .. } => {
                Ok(snapshot.contains_key(&column_family.keyspace, key)?)
            }
            ReaderKind::Transaction { snapshot, overlay } => {
                match overlay.get(column_family.index, key) {
                    Some(value) => Ok(value.is_some()),
                    None => Ok(snapshot.contains_key(&column_family.keyspace, key)?),
                }
            }
        }
    }

    /// Iterates over all keys of a column family, in increasing byte order.
    #[expect(clippy::iter_not_returning_iterator)]
    pub fn iter(&self, column_family: &ColumnFamily) -> Iter<'a> {
        self.scan_prefix(column_family, &[])
    }

    /// Iterates over the keys starting with `prefix`, in increasing byte order.
    pub fn scan_prefix(&self, column_family: &ColumnFamily, prefix: &[u8]) -> Iter<'a> {
        match &self.kind {
            ReaderKind::Snapshot { snapshot, db } => Iter {
                cursor: Box::new(Cursor::new(
                    snapshot.prefix(&column_family.keyspace, prefix),
                    None,
                )),
                _db: Some(Arc::clone(db)),
            },
            ReaderKind::Transaction { snapshot, overlay } => {
                let pending: &'a Overlay = overlay;
                Iter {
                    cursor: Box::new(Cursor::new(
                        snapshot.prefix(&column_family.keyspace, prefix),
                        pending.scan_prefix(column_family.index, prefix),
                    )),
                    _db: None,
                }
            }
        }
    }

    /// Number of keys in a column family.
    pub fn len(&self, column_family: &ColumnFamily) -> Result<usize, KvError> {
        let mut count = 0;
        let mut iter = self.iter(column_family);
        while iter.is_valid() {
            count += 1;
            iter.next();
        }
        iter.status()?;
        Ok(count)
    }

    /// Whether a column family has no keys.
    pub fn is_empty(&self, column_family: &ColumnFamily) -> Result<bool, KvError> {
        let iter = self.iter(column_family);
        iter.status()?;
        Ok(!iter.is_valid())
    }
}

/// Write-only batch. Nothing is visible until [`Transaction::commit`]; dropping
/// it discards the writes.
pub struct Transaction {
    db: Arc<DbInner>,
    overlay: Overlay,
}

impl Transaction {
    /// Sets `key` to `value`.
    pub fn insert(&mut self, column_family: &ColumnFamily, key: &[u8], value: &[u8]) {
        self.overlay.insert(column_family.index, key, value);
    }

    /// Sets `key` to an empty value.
    pub fn insert_empty(&mut self, column_family: &ColumnFamily, key: &[u8]) {
        self.insert(column_family, key, &[]);
    }

    /// Removes `key`.
    pub fn remove(&mut self, column_family: &ColumnFamily, key: &[u8]) {
        self.overlay.remove(column_family.index, key);
    }

    /// Removes every key in `start_key..end_key` (end excluded), as of commit
    /// time. Keys written after this call in the same transaction are kept.
    pub fn remove_range(&mut self, column_family: &ColumnFamily, start_key: &[u8], end_key: &[u8]) {
        self.overlay
            .remove_range(column_family.index, start_key, end_key);
    }

    /// Applies all writes atomically.
    pub fn commit(self) -> Result<(), KvError> {
        self.db.commit(&self.overlay)
    }
}

/// Transaction reading a snapshot plus its own pending writes.
///
/// Writes are visible to [`ReadableTransaction::reader`] immediately (including
/// in iteration order) and to other readers only after
/// [`ReadableTransaction::commit`]. Dropping it discards the writes. As with
/// the RocksDB backend, there is no conflict detection: the last commit wins.
pub struct ReadableTransaction<'a> {
    db: &'a DbInner,
    snapshot: fjall::Snapshot,
    overlay: Overlay,
}

impl ReadableTransaction<'_> {
    /// View of the snapshot merged with the pending writes.
    pub fn reader(&self) -> Reader<'_> {
        Reader {
            kind: ReaderKind::Transaction {
                snapshot: &self.snapshot,
                overlay: &self.overlay,
            },
        }
    }

    /// Sets `key` to `value`.
    pub fn insert(&mut self, column_family: &ColumnFamily, key: &[u8], value: &[u8]) {
        self.overlay.insert(column_family.index, key, value);
    }

    /// Sets `key` to an empty value.
    pub fn insert_empty(&mut self, column_family: &ColumnFamily, key: &[u8]) {
        self.insert(column_family, key, &[]);
    }

    /// Removes `key`.
    pub fn remove(&mut self, column_family: &ColumnFamily, key: &[u8]) {
        self.overlay.remove(column_family.index, key);
    }

    /// Applies all writes atomically.
    pub fn commit(self) -> Result<(), KvError> {
        self.db.commit(&self.overlay)
    }
}

/// A value read from the database, shared with fjall's cache without copy.
pub struct PinnableSlice(Slice);

impl Deref for PinnableSlice {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.0
    }
}

impl AsRef<[u8]> for PinnableSlice {
    fn as_ref(&self) -> &[u8] {
        self
    }
}

impl Borrow<[u8]> for PinnableSlice {
    fn borrow(&self) -> &[u8] {
        self
    }
}

/// Copies the value. Oxigraph 0.5's `rocksdb_wrapper.rs` has this
/// conversion (its `rocksdb.rs` uses it to read `id2str`); 0.6 dropped it.
impl From<PinnableSlice> for Vec<u8> {
    fn from(value: PinnableSlice) -> Self {
        value.to_vec()
    }
}

/// Cursor over keys in increasing byte order.
///
/// Starts positioned on the first key (if any). Not a Rust [`Iterator`]
/// because `key` borrows from the cursor.
pub struct Iter<'a> {
    // Boxed: Oxigraph keeps several iterators inline in its own enums
    cursor: Box<Cursor<'a>>,
    /// Keeps a read-only handle's private copy alive.
    _db: Option<Arc<DbInner>>,
}

impl Iter<'_> {
    /// Whether the cursor is on a key.
    pub fn is_valid(&self) -> bool {
        self.cursor.key().is_some()
    }

    /// Reports a read error that made the cursor stop early.
    pub fn status(&self) -> Result<(), KvError> {
        self.cursor.status()
    }

    /// Moves to the next key.
    pub fn next(&mut self) {
        self.cursor.advance();
    }

    /// The current key, or `None` once the cursor is exhausted.
    pub fn key(&self) -> Option<&[u8]> {
        self.cursor.key()
    }
}

/// Writer for a sorted file to be added with [`Db::insert_stt_files`].
pub struct SstFileWriter(sst::Writer);

impl SstFileWriter {
    /// Appends a key. Keys must be inserted in strictly increasing order.
    pub fn insert(&mut self, key: &[u8], value: &[u8]) -> Result<(), KvError> {
        self.0.insert(key, value)
    }

    /// Appends a key with an empty value.
    pub fn insert_empty(&mut self, key: &[u8]) -> Result<(), KvError> {
        self.insert(key, &[])
    }

    /// Finishes the file and returns its path.
    pub fn finish(self) -> Result<PathBuf, KvError> {
        self.0.finish()
    }
}

#[cfg(test)]
mod tests;
