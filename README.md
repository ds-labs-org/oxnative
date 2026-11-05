# oxnative

Pure-Rust storage backend for [Oxigraph](https://github.com/oxigraph/oxigraph),
used through the `oxnative` feature of the `oxigraph` crate in the
[contreforts-ai/oxigraph](https://github.com/contreforts-ai/oxigraph) fork:

```sh
# in the oxigraph fork
cargo build -p oxigraph --no-default-features --features oxnative
```

**Status: placeholder.** This crate only defines the API that Oxigraph's on-disk
storage (`lib/oxigraph/src/storage/rocksdb.rs`) uses. `Db::open_read_write` and
`Db::open_read_only` return `KvError::Unimplemented`, so every other method is
unreachable. Replace the bodies with a real engine.

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
versa; migrate with `oxigraph dump` / `oxigraph load`.

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
With the placeholder, the 12 on-disk tests in `lib/oxigraph/tests/store.rs` and
`storage::rocksdb::tests::test_transaction` fail with `Unimplemented`; all
in-memory tests pass. `test_backward_compatibility` is ignored under `oxnative`
because its fixture is in RocksDB's on-disk format.

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache 2.0](LICENSE-APACHE), like
Oxigraph.
