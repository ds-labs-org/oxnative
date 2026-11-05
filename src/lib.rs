#![cfg_attr(doc, doc = include_str!("../README.md"))]
//!
//! Every handle type holds a value of the uninhabited private `Never` type, so none
//! of them can exist until a real engine replaces it. That keeps the placeholder
//! free of `unimplemented!()`: the only reachable code paths are the two `open`
//! functions, which return [`KvError::Unimplemented`].

use std::borrow::Borrow;
use std::error::Error as StdError;
use std::marker::PhantomData;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::{fmt, io};

/// Placeholder for the engine state. Uninhabited: replace it with real handles.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Never {}

/// Definition of a column family, given when opening the database.
///
/// The set of column families is fixed for the lifetime of a [`Db`].
pub struct ColumnFamilyDefinition {
    /// Name of the column family.
    pub name: &'static str,
    /// Whether the column family is iterated over (RocksDB uses this to tune
    /// its memtable and block cache; an engine may ignore it).
    pub use_iter: bool,
    /// Length of the key prefix used by prefix scans (RocksDB builds a prefix
    /// bloom filter of this length; an engine may ignore it).
    pub min_prefix_size: usize,
    /// Whether writes to this column family may be reordered (an engine may
    /// ignore it).
    pub unordered_writes: bool,
}

/// Options for [`Db::open_read_write`].
#[derive(Clone, Copy, Debug, Default)]
pub struct DbOptions {
    /// Maximum number of files the engine may keep open.
    pub max_open_files: Option<i32>,
    /// Number of file descriptors to leave to the rest of the process.
    pub fd_reserve: Option<u32>,
}

/// A database handle. Cheap to clone, `Send + Sync`.
#[derive(Clone)]
pub struct Db {
    never: Never,
}

impl Db {
    /// Opens or creates the database at `path` in read-write mode.
    ///
    /// Column families listed in `column_families` that do not exist yet are
    /// created.
    pub fn open_read_write(
        _path: &Path,
        _column_families: Vec<ColumnFamilyDefinition>,
        _db_options: DbOptions,
    ) -> Result<Self, KvError> {
        Err(KvError::Unimplemented)
    }

    /// Opens an existing database at `path` in read-only mode.
    ///
    /// Writes, transactions, bulk loads, flush and compaction must fail on a
    /// read-only handle.
    pub fn open_read_only(
        _path: &Path,
        _column_families: Vec<ColumnFamilyDefinition>,
    ) -> Result<Self, KvError> {
        Err(KvError::Unimplemented)
    }

    /// Whether this handle was opened with [`Db::open_read_write`].
    pub fn is_writable(&self) -> bool {
        match self.never {}
    }

    /// Returns the handle of a column family given at open time.
    ///
    /// Must fail with [`KvError::MissingColumnFamily`] if the column family
    /// does not exist.
    pub fn column_family(&self, _name: &'static str) -> Result<ColumnFamily, KvError> {
        match self.never {}
    }

    /// Returns a point-in-time view of the database.
    ///
    /// Later writes must not be visible through it. It may outlive this `Db`
    /// handle (but not the last clone of it).
    #[must_use]
    pub fn snapshot(&self) -> Reader<'static> {
        match self.never {}
    }

    /// Starts a write-only batch, applied atomically by [`Transaction::commit`].
    pub fn start_transaction(&self) -> Result<Transaction, KvError> {
        match self.never {}
    }

    /// Starts a transaction that reads a snapshot taken now plus its own
    /// pending writes, applied atomically by [`ReadableTransaction::commit`].
    pub fn start_readable_transaction(&self) -> Result<ReadableTransaction<'_>, KvError> {
        match self.never {}
    }

    /// Reads the latest committed value of `key`.
    pub fn get(
        &self,
        _column_family: &ColumnFamily,
        _key: &[u8],
    ) -> Result<Option<PinnableSlice>, KvError> {
        match self.never {}
    }

    /// Whether `key` exists in the latest committed state.
    pub fn contains_key(
        &self,
        _column_family: &ColumnFamily,
        _key: &[u8],
    ) -> Result<bool, KvError> {
        match self.never {}
    }

    /// Writes a single key outside of any batch.
    pub fn insert(
        &self,
        _column_family: &ColumnFamily,
        _key: &[u8],
        _value: &[u8],
    ) -> Result<(), KvError> {
        match self.never {}
    }

    /// Persists all pending writes to disk.
    pub fn flush(&self) -> Result<(), KvError> {
        match self.never {}
    }

    /// Compacts a column family. May be a no-op.
    pub fn compact(&self, _column_family: &ColumnFamily) -> Result<(), KvError> {
        match self.never {}
    }

    /// Creates a writer for a sorted file, written outside the database and
    /// added later with [`Db::insert_stt_files`]. Keys are inserted in
    /// increasing order.
    pub fn new_sst_file(&self) -> Result<SstFileWriter, KvError> {
        match self.never {}
    }

    /// Atomically adds files produced by [`SstFileWriter::finish`] to the
    /// given column families. An empty list is a no-op.
    pub fn insert_stt_files(
        &self,
        _ssts_for_cf: &[(ColumnFamily, PathBuf)],
    ) -> Result<(), KvError> {
        match self.never {}
    }

    /// Writes a consistent copy of the database into `target_directory`, which
    /// must not exist yet. The copy must be openable with
    /// [`Db::open_read_write`] and [`Db::open_read_only`].
    pub fn backup(&self, _target_directory: &Path) -> Result<(), KvError> {
        match self.never {}
    }
}

/// Handle to a column family. Cheap to clone, `Send + Sync`, usable as a map key.
#[derive(Clone, Eq, PartialEq, Hash)]
pub struct ColumnFamily {
    never: Never,
}

/// A read view: a snapshot ([`Db::snapshot`]) or a transaction's own view
/// ([`ReadableTransaction::reader`]). Cheap to clone, `Send + Sync`.
#[derive(Clone)]
pub struct Reader<'a> {
    never: Never,
    /// Stand-in for the engine's shared state (a real reader keeps the
    /// database alive).
    _db: Arc<()>,
    _lifetime: PhantomData<&'a ()>,
}

impl<'a> Reader<'a> {
    /// Reads `key` in this view.
    pub fn get(
        &self,
        _column_family: &ColumnFamily,
        _key: &[u8],
    ) -> Result<Option<PinnableSlice>, KvError> {
        match self.never {}
    }

    /// Whether `key` exists in this view.
    pub fn contains_key(
        &self,
        _column_family: &ColumnFamily,
        _key: &[u8],
    ) -> Result<bool, KvError> {
        match self.never {}
    }

    /// Iterates over all keys of a column family, in increasing byte order.
    #[expect(clippy::iter_not_returning_iterator)]
    pub fn iter(&self, _column_family: &ColumnFamily) -> Iter<'a> {
        match self.never {}
    }

    /// Iterates over the keys starting with `prefix`, in increasing byte order.
    pub fn scan_prefix(&self, _column_family: &ColumnFamily, _prefix: &[u8]) -> Iter<'a> {
        match self.never {}
    }

    /// Number of keys in a column family.
    pub fn len(&self, _column_family: &ColumnFamily) -> Result<usize, KvError> {
        match self.never {}
    }

    /// Whether a column family has no keys.
    pub fn is_empty(&self, _column_family: &ColumnFamily) -> Result<bool, KvError> {
        match self.never {}
    }
}

/// Write-only batch. Nothing is visible until [`Transaction::commit`]; dropping
/// it discards the writes.
pub struct Transaction {
    never: Never,
}

impl Transaction {
    /// Sets `key` to `value`.
    pub fn insert(&mut self, _column_family: &ColumnFamily, _key: &[u8], _value: &[u8]) {
        match self.never {}
    }

    /// Sets `key` to an empty value.
    pub fn insert_empty(&mut self, column_family: &ColumnFamily, key: &[u8]) {
        self.insert(column_family, key, &[]);
    }

    /// Removes `key`.
    pub fn remove(&mut self, _column_family: &ColumnFamily, _key: &[u8]) {
        match self.never {}
    }

    /// Removes every key in `start_key..end_key` (end excluded).
    pub fn remove_range(
        &mut self,
        _column_family: &ColumnFamily,
        _start_key: &[u8],
        _end_key: &[u8],
    ) {
        match self.never {}
    }

    /// Applies all writes atomically.
    pub fn commit(self) -> Result<(), KvError> {
        match self.never {}
    }
}

/// Transaction reading a snapshot plus its own pending writes.
///
/// Writes are visible to [`ReadableTransaction::reader`] immediately (including
/// in iteration order) and to other readers only after
/// [`ReadableTransaction::commit`]. Dropping it discards the writes.
pub struct ReadableTransaction<'a> {
    never: Never,
    _lifetime: PhantomData<&'a ()>,
}

impl ReadableTransaction<'_> {
    /// View of the snapshot merged with the pending writes.
    pub fn reader(&self) -> Reader<'_> {
        match self.never {}
    }

    /// Sets `key` to `value`.
    pub fn insert(&mut self, _column_family: &ColumnFamily, _key: &[u8], _value: &[u8]) {
        match self.never {}
    }

    /// Sets `key` to an empty value.
    pub fn insert_empty(&mut self, column_family: &ColumnFamily, key: &[u8]) {
        self.insert(column_family, key, &[]);
    }

    /// Removes `key`.
    pub fn remove(&mut self, _column_family: &ColumnFamily, _key: &[u8]) {
        match self.never {}
    }

    /// Applies all writes atomically.
    pub fn commit(self) -> Result<(), KvError> {
        match self.never {}
    }
}

/// A value read from the database. Ideally borrowed from the engine's pages or
/// cache rather than copied.
pub struct PinnableSlice {
    never: Never,
}

impl Deref for PinnableSlice {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        match self.never {}
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

/// Cursor over keys in increasing byte order.
///
/// Starts positioned on the first key (if any). Not a Rust [`Iterator`]
/// because `key` borrows from the cursor.
pub struct Iter<'a> {
    never: Never,
    /// Stand-ins for what a real cursor holds: the view it reads, its current
    /// key and its upper bound. They give the placeholder a realistic size.
    _reader: Reader<'a>,
    _current_key: Vec<u8>,
    _upper_bound: Option<Vec<u8>>,
}

impl Iter<'_> {
    /// Whether the cursor is on a key.
    pub fn is_valid(&self) -> bool {
        match self.never {}
    }

    /// Reports a read error that made the cursor stop early.
    pub fn status(&self) -> Result<(), KvError> {
        match self.never {}
    }

    /// Moves to the next key.
    pub fn next(&mut self) {
        match self.never {}
    }

    /// The current key, or `None` once the cursor is exhausted.
    pub fn key(&self) -> Option<&[u8]> {
        match self.never {}
    }
}

/// Writer for a sorted file to be added with [`Db::insert_stt_files`].
pub struct SstFileWriter {
    never: Never,
}

impl SstFileWriter {
    /// Appends a key. Keys must be inserted in strictly increasing order.
    pub fn insert(&mut self, _key: &[u8], _value: &[u8]) -> Result<(), KvError> {
        match self.never {}
    }

    /// Appends a key with an empty value.
    pub fn insert_empty(&mut self, key: &[u8]) -> Result<(), KvError> {
        self.insert(key, &[])
    }

    /// Finishes the file and returns its path.
    pub fn finish(self) -> Result<PathBuf, KvError> {
        match self.never {}
    }
}

/// Errors returned by the engine.
#[derive(Debug)]
#[non_exhaustive]
pub enum KvError {
    /// An I/O error.
    Io(io::Error),
    /// The stored data is invalid.
    Corruption(Box<dyn StdError + Send + Sync + 'static>),
    /// The database has no column family with this name.
    MissingColumnFamily(&'static str),
    /// Any other error, e.g. writing through a read-only handle.
    Other(Box<dyn StdError + Send + Sync + 'static>),
    /// The placeholder engine has not been implemented yet.
    Unimplemented,
}

impl fmt::Display for KvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => e.fmt(f),
            Self::Corruption(e) | Self::Other(e) => e.fmt(f),
            Self::MissingColumnFamily(name) => write!(f, "Column family {name} does not exist"),
            Self::Unimplemented => {
                f.write_str("the oxnative storage backend is not implemented yet")
            }
        }
    }
}

impl StdError for KvError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Corruption(e) | Self::Other(e) => Some(&**e),
            Self::MissingColumnFamily(_) | Self::Unimplemented => None,
        }
    }
}

impl From<io::Error> for KvError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}
