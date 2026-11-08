# oxnative

Pure-Rust storage backend for [Oxigraph](https://github.com/oxigraph/oxigraph),
used through the `oxnative` feature of the `oxigraph` crate in the
[contreforts-ai/oxigraph](https://github.com/contreforts-ai/oxigraph) fork:

```sh
# in the oxigraph fork
cargo build -p oxigraph --no-default-features --features oxnative
```

**Status: first working version**, built on [fjall](https://github.com/fjall-rs/fjall)
3, a pure-Rust LSM-tree engine. Oxigraph's whole test suite passes on it, except
`test_backward_compatibility`, whose fixture is a RocksDB database. Requires Rust
1.90 (fjall's minimum), against 1.87 for Oxigraph itself.

## Contract

The API mirrors `lib/oxigraph/src/storage/rocksdb_wrapper.rs`, the safe wrapper
around RocksDB. Oxigraph's indexes and key encoding stay in Oxigraph; this crate
is only an ordered key-value engine with:

- a fixed set of named column families, given at open time;
- point `get` / `contains_key`;
- point-in-time snapshots (`Db::snapshot`) that are `Clone + Send + Sync` and
  outlive the borrow of `Db` (`Reader<'static>`);
- ordered iteration over keys, whole column family or by prefix;
- atomic write batches with `insert`, `remove`, `remove_range` (`Transaction`);
- read-your-writes transactions: a snapshot plus pending writes, visible to
  `get` and to iteration through `ReadableTransaction::reader`;
- bulk loading: sorted files written outside the database
  (`Db::new_sst_file`), then added to several column families atomically
  (`Db::insert_stt_files`);
- `flush`, `compact` (may be no-ops), `backup` into a new directory, and
  read-only opening.

Errors map onto Oxigraph's `StorageError` in
`lib/oxigraph/src/storage/oxnative_wrapper.rs`: `KvError::Io` → `StorageError::Io`,
`KvError::Corruption` and `KvError::MissingColumnFamily` →
`StorageError::Corruption`, everything else → `StorageError::Other`.

Data written by this backend is not readable by the RocksDB backend and vice
versa; migrate with `oxigraph dump` / `oxigraph load`. Opening a non-empty
directory without an `OXNATIVE` file (such as a RocksDB store) fails instead of
silently starting an empty database.

## Design

On-disk layout of a database directory:

| Path       | Content                                                     |
|------------|-------------------------------------------------------------|
| `OXNATIVE` | Format marker, checked on every open.                       |
| `fjall/`   | The fjall database: one keyspace per column family, plus `_oxnative_meta`. |
| `bulk/`    | Bulk-load files, until the loader deletes them.             |

How each part of the contract maps onto fjall:

- **Column families** are fjall keyspaces. A `default` column family always
  exists, as in RocksDB. `ColumnFamilyDefinition`'s tuning hints and
  `DbOptions::fd_reserve` are ignored; `max_open_files` sets fjall's file cache.
- **Snapshots** are fjall snapshots, consistent across keyspaces.
- **`Transaction`** buffers its writes and commits them as one fjall batch,
  which is atomic across keyspaces. Writes to the same key are folded first,
  because every item of a fjall batch gets the same sequence number.
  `remove_range` has no fjall equivalent: at commit, the keys of the range are
  read from the committed state and removed, so its cost is proportional to the
  number of keys removed. Commits are serialized by a lock so that this read
  is exact.
- **`ReadableTransaction`** is a snapshot plus the pending writes in a
  `BTreeMap`; its reader merges both, for point reads and in iteration order.
  As with the RocksDB backend there is no conflict detection: the last commit
  wins.
- **Bulk loads**: `SstFileWriter` writes a private sorted file format.
  `insert_stt_files` merges the files of each column family (the last file wins
  for duplicate keys) into a fjall ingestion, which gets a newer sequence number
  than existing data, so it overrides earlier values and removals. fjall
  ingests one keyspace at a time, so two mechanisms restore RocksDB's atomic
  multi-column-family ingestion:
  - readers never see a partial load: new snapshots and latest-state reads wait
    while the column families are ingested;
  - a crash never leaves a partial load: the list of files is written durably to
    `_oxnative_meta` before ingesting and removed after. The next read-write open
    re-ingests the listed files (idempotent) and deletes them.
- **Read-only handles**: fjall locks its directory and has no read-only mode,
  and Oxigraph opens read-only handles while a read-write handle is open on the
  same directory. `open_read_only` therefore opens a private copy next to the
  database (or in the system temporary directory): table files, which lsm-tree
  never modifies once written, are hard-linked, and the rest (journals,
  manifests) is copied. The handle sees the database as it was when opened, and
  the copy is deleted when the handle is dropped.
- **`backup`** writes a new database from a snapshot. It is consistent even
  while background compaction runs.

## Vaulted column families (feature `vault`)

Optional encryption at rest for chosen column families. It is behind the Cargo
feature `vault`, which is off by default: without it the crate has no extra
dependency and behaves exactly as before.

```sh
cargo build --features vault
cargo test --features vault
```

### Threat model

Protected: an attacker who obtains the database directory, a `backup()` of it
or a bulk-load file (disk theft, stolen backup, shared snapshot) without the
key cannot read the **values** of the vault column families, and cannot forge
or alter a value, or move one to another key or column family, without
detection. Authenticity is per value only (see "Integrity" below).

Not protected:

- **Keys are plaintext**, in every column family. They must stay plaintext so
  that ordering, range scans and prefix scans keep working. Do not vault a
  column family whose keys are themselves secret; the number of records, the
  key lengths and the sizes of the values (plaintext length plus 42 bytes) are
  visible too.
- Plain column families and the `_oxnative_meta` keyspace are not encrypted.
- A running process: the key and the decrypted values are in its memory, and
  anyone who can read it or call the API has the data. Every `VaultKey` is
  zeroized when dropped, which is best effort.
- Integrity is limited to the pair (column family name, record key). The
  associated data does not contain a version, a database identifier or any
  other context, so:
  - a value can be **replayed**: an older sealed value copied back under its
    own key and column family opens without error, and returns stale data;
  - sealed bytes can be transplanted between databases that share a key: a
    sealed value copied from one database into the same column family and key
    of another database with the same key is accepted;
  - restoring an older copy of the whole directory is valid, for the same
    reason.
  Use a distinct key per database to rule out transplants.
- Key management: this crate takes the 32 bytes and nothing else. Where the key
  is stored, how it is derived and who may read it is the caller's business.

### What it does

- Cipher: XChaCha20-Poly1305 (RustCrypto), with a random 24-byte nonce per
  value.
- Only values are encrypted. The associated data is the column family name and
  the record key, so a sealed value copied under another key or into another
  column family fails to open. It does not bind a version or a database, see
  the threat model.
- Fail closed: a missing key, a wrong key (checked at open with a sealed key
  check stored in `_oxnative_meta`), a damaged value or a vault column family
  name that is not among the column families all return an error. Nothing is
  ever returned as garbage or as plaintext fallback.
- Plain and vault column families coexist in one database.
- A database is vaulted or plain for its whole life, decided at creation. A
  plain database is never converted, and a vaulted one cannot be opened
  without its key (nor by a build without the feature).
- `Db::backup` copies vault values as stored, still sealed, and the backup
  opens with the same key, through `Db::open_read_write_with_vault` or
  `Db::open_read_only_with_vault` (the plain `open_*` functions refuse it). `Db::new_sst_file_for` seals the values of a bulk
  file of a vault column family; such a file only loads into that column
  family of a database with the same key.
- No rotation yet: the sealed format reserves a key id, so it can be added
  without a format change (backlog #9).

### API

```rust,ignore
use oxnative::{ColumnFamilyDefinition, Db, DbOptions, VaultKey, VaultOptions};

let key = VaultKey::new(key_bytes); // [u8; 32]; VaultKey::try_from(&[u8]) checks the length
let db = Db::open_read_write_with_vault(
    path,
    column_families, // Vec<ColumnFamilyDefinition>, including "id2str"
    DbOptions::default(),
    VaultOptions {
        key,
        vault_column_families: vec!["id2str"],
    },
)?;
// then use `get`, `insert`, transactions, `iter`, `scan_prefix`, `new_sst_file_for`
// as usual: sealing and opening are transparent.

// Read-only handles of a vaulted database need the key too:
// Db::open_read_only_with_vault(path, column_families, vault_options)
```

`DbOptions` is identical with and without the feature (still `Copy`, no vault
field): the feature is additive. `Db::open_read_only` on a vaulted database, and
`Db::open_read_write` on one, fail.
For bulk loads of a vault column family use `new_sst_file_for(&cf)`: a file
from `new_sst_file()` would hold plaintext, so it fails on a database with vault
column families (and `insert_stt_files` refuses such files for them).

### On-disk format

- `OXNATIVE` marker of a vaulted database, written by the current version:

  ```text
  oxnative 2
  engine fjall 3
  vault 1
  ```

  The first line differs from the `oxnative 1` of plain databases, so builds
  without vault support, which only compare the first line, refuse to open it.
  Vaulted databases created earlier with `oxnative 1` as first line (and a
  `vault 1` line) still open; a `vault` line with another version is refused.
  A plain database keeps the two-line marker `oxnative 1` / `engine fjall 3`.
- Each stored value of a vault column family is
  `format version (1 byte, 1) || key id (1 byte, 0) || nonce (24) || ciphertext || tag (16)`:
  42 bytes of overhead. Values are not compressible after encryption, so the
  LZ4 setting does not help on them.
- Associated data: `format version || key id || len(column family) as u64 LE
  || column family name || record key`.
- `_oxnative_meta` holds `vault_check`, a sealed constant bound to its location,
  which tells a wrong key from a right one without reading user data, and
  `vault_cfs`, the sorted set of vault column families the check is bound to. An
  open whose set differs is refused.
- Bulk-load files of a vault column family carry sealed values and the column
  family name.

### Using it from the Oxigraph fork

Vaulting **`id2str`** (the dictionary from the 128-bit hash to the string of
every IRI, blank node and literal) alone protects much less than it seems:

- The fork stores terms of at most 15 bytes inline in the encoded terms, hence
  in the plaintext keys of the index column families, and never writes them to
  `id2str`. Short IRIs, short strings, and all numeric, boolean, date, time and
  duration literals are therefore in clear text in the index keys. Only terms
  over 15 bytes reach `id2str`.
- For those, the key of the other terms is a hash, `StrHash`, which is plain
  unkeyed SipHash-2-4. Anyone with the database can hash a guessed value (a
  known IRI, a name, an e-mail address) and look for the hash in the index
  keys: equality of guessable values is confirmable, vaulted or not. Only long
  values that cannot be guessed are really protected.
- The structure of the graph (which hashes occur together, in which graphs)
  stays visible.

Real protection of term contents needs changes in the fork, not only vaulting:
no inlining (every term goes through `id2str`), and a keyed hash for `StrHash`
derived from the vault key instead of the unkeyed SipHash. Without them, treat
the vault as protecting long, unguessable terms only.

The nine index column families (`spog`, `posg`, `ospg`, `gspo`, `gpos`, `gosp`,
`dspo`, `dpos`, `dosp`) keep the encoded terms in their keys and have empty
values, so vaulting them protects nothing. Leave them, and `default`/`graphs`,
plain unless the graph names themselves are secret (the `graphs` keys are the
encoded graph names, also plaintext).

In the fork, in `oxnative_wrapper.rs`, call `Db::open_read_write_with_vault` and
`Db::open_read_only_with_vault` for read-only handles; `Store::open` and
friends take the key from the caller, so the fork needs an opening function
that accepts it. Its bulk loader must create the `id2str` files with
`new_sst_file_for`. The fork's pure-Rust build turns the feature on with
`oxnative = { ..., features = ["vault"] }`, and its CI runs with and without it.

## Known limitations

- `open_read_only` costs a copy of the journals and of the metadata files, and
  a full copy of the tables when hard links are impossible (another file
  system). A process that crashes leaves its copy behind, as a hidden
  `.<name>.oxnative-ro-*` directory next to the database.
- `backup` rewrites every key, so it takes time and space proportional to the
  database, where RocksDB hard-links its table files.
- While a bulk load is being committed, new snapshots and writes wait for it.
  The merge of the bulk files happens at that point, in one thread, whereas
  RocksDB only links the files it is given.
- All commits go through one lock.
- On-disk size, for 500,000 triples (28 MB of N-Triples): about 34 MB after a
  bulk load, against 28 MB with RocksDB. fjall's journal file is preallocated to
  64 MiB, so tools reporting apparent sizes (`ls -l`, `du --apparent-size`)
  show about 64 MB more than is allocated (`du`). Data blocks are LZ4-compressed
  at every level (see `keyspace_options`); this only applies to databases
  created with this setting, since fjall stores the options with each keyspace.
- Commits are written to the OS but not synced until `Db::flush`, like
  RocksDB's default write-ahead log settings; fjall also syncs when the database
  is dropped.

## Open fjall issues that affect this backend

Checked on 2026-11-05 against fjall 3.1.13 / lsm-tree 3.1.10.

- [fjall#333](https://github.com/fjall-rs/fjall/issues/333): a snapshot opened
  while a batch is being committed can see part of it, if a flush, compaction
  or ingestion finishes at that moment. This breaks the atomicity of
  `Transaction::commit` for concurrent readers.
- [fjall#288](https://github.com/fjall-rs/fjall/issues/288), fix proposed in
  [fjall#325](https://github.com/fjall-rs/fjall/pull/325): a keyspace whose
  memtables are empty and whose tables were dropped (for example all its keys
  removed, then `compact`) pins the oldest journal, and the journal then grows
  without bound. Oxigraph's `optimize` compacts every column family, so a store
  whose named graphs are all removed, then optimized, can hit it.
- [fjall#344](https://github.com/fjall-rs/fjall/issues/344): the write buffer
  size limit is not enforced, so memtables and the journal can grow well past
  64 MiB during sustained writes (bulk loads use ingestion and are not
  affected).
- [fjall#313](https://github.com/fjall-rs/fjall/issues/313) (read-only mode)
  and [fjall#52](https://github.com/fjall-rs/fjall/issues/52) /
  [fjall#314](https://github.com/fjall-rs/fjall/issues/314) (checkpoints): once
  available, they would replace the private copy behind `open_read_only` and the
  logical `backup`.

## How it plugs into Oxigraph

Features of the `oxigraph` crate in the fork:

| Feature    | Meaning                                                           |
|------------|-------------------------------------------------------------------|
| `rocksdb`  | RocksDB backend (C++, `oxrocksdb-sys`), as upstream. In `default`. |
| `oxnative` | This backend. Wins over `rocksdb` when both are enabled.          |

A pure-Rust build is `--no-default-features --features oxnative`: it enables
nothing named RocksDB, and `oxrocksdb-sys` is not in its dependency graph.

Either feature enables on-disk storage (`Store::open` and friends). Upstream
gates it on `feature = "rocksdb"`; the fork gates it on `cfg(oxigraph_on_disk)`,
set by `lib/oxigraph/build.rs` when `rocksdb` or `oxnative` is enabled. The
backend is picked in `lib/oxigraph/src/storage/mod.rs`: with `oxnative`, the
module `rocksdb_wrapper` is loaded from `oxnative_wrapper.rs` (forwards to this
crate and converts `KvError` into `StorageError`) instead of
`rocksdb_wrapper.rs` (RocksDB FFI).

When a rebase on upstream conflicts on one of these gates, take upstream's line
and replace `feature = "rocksdb"` with `oxigraph_on_disk` again; the only
`feature = "rocksdb"` left in `lib/oxigraph` is the backend selection in
`storage/mod.rs`.

The fork depends on this repository through git, in its workspace
`Cargo.toml`: its branch based on upstream `main` (0.6) follows this
repository's `main`, and its `oxnative-0.5` branch (upstream v0.5.9 plus the
`oxnative` feature) pins a commit with `rev`. To develop both side by side,
clone this repository next to the fork and add to the fork's workspace
`Cargo.toml`:

```toml
[patch."https://github.com/ds-labs-org/oxnative"]
oxnative = { path = "../oxnative" }
```

After pushing here, run `cargo update -p oxnative` in the fork to move its
`Cargo.lock` to the new commit.

## Keeping the API in sync with upstream Oxigraph

When the fork is rebased on upstream, the only file to watch is
`lib/oxigraph/src/storage/rocksdb_wrapper.rs`: any method added or changed there
must be mirrored here and in the fork's `oxnative_wrapper.rs`. The fork's CI runs
`cargo clippy --no-default-features --features oxnative`, so a mismatch fails its
build. This crate keeps the union of what the fork's branches need: for example
`From<PinnableSlice> for Vec<u8>`, which only Oxigraph 0.5 uses.

## Acceptance tests

In the fork: `cargo test -p oxigraph --no-default-features --features oxnative`.
Everything passes; `test_backward_compatibility` is ignored under `oxnative`
because its fixture is in RocksDB's on-disk format. This crate's own tests
(`cargo test` here) cover the engine-level edge cases: merged iteration with
pending removals, `remove_range` ordering, overlapping bulk files, recovery of
an interrupted bulk load, read-only isolation and refusing foreign directories.

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache 2.0](LICENSE-APACHE), like
Oxigraph.
