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
- Commits are written to the OS but not synced until `Db::flush`, like
  RocksDB's default write-ahead log settings; fjall also syncs when the database
  is dropped.

## How it plugs into Oxigraph

Features of the `oxigraph` crate in the fork:

| Feature       | Meaning                                                      |
|---------------|--------------------------------------------------------------|
| `rocksdb`     | On-disk storage (`Store::open` …). Needs one backend below.  |
| `rocksdb-cpp` | RocksDB backend (C++, `oxrocksdb-sys`). In `default`.         |
| `oxnative`    | This backend. Wins over `rocksdb-cpp` when both are enabled.  |

`rocksdb` keeps upstream's meaning for every `cfg(feature = "rocksdb")`, so none
of them had to change. The switch is in `lib/oxigraph/src/storage/mod.rs`: with
`oxnative`, the module `rocksdb_wrapper` is loaded from `oxnative_wrapper.rs`
(forwards to this crate and converts `KvError` into `StorageError`) instead of
`rocksdb_wrapper.rs` (RocksDB FFI).

The fork depends on this repository's `main` branch through git
(`oxnative = { git = "https://github.com/ds-labs-org/oxnative", branch = "main" }`
in its workspace `Cargo.toml`). To develop both side by side, clone this
repository next to the fork and add to the fork's workspace `Cargo.toml`:

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
build.

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
