# RDF triple/quad store storage for SPARQL performance

Survey of published research, production systems and source code on how RDF
stores lay out data on disk, and what that means for an ordered key-value
engine such as `oxnative`. Status: November 2026. References are in the last
section, numbered `[n]`.

Scope:

1. What a SPARQL engine asks of storage.
2. Index layouts used by research and production systems.
3. Term dictionaries and key encoding.
4. Persistence engines: LSM trees, B+trees (in-place and copy-on-write),
   compressed immutable structures, column stores, relational back ends.
5. Join algorithms and their storage requirements.
6. Benchmarks and published comparisons.
7. Consequences for `oxnative`.

---

## 1. Workload model

A SPARQL basic graph pattern (BGP) is a set of triple or quad patterns. Each
position is either a constant or a variable, so for quads there are 2^4 = 16
binding shapes, for triples 2^3 = 8. Evaluation is dominated by:

- **Range scans** on a prefix of bound positions (`?s rdf:type :C`,
  `:x ?p ?o`). These need an index whose sort order starts with the bound
  positions.
- **Joins** between pattern results, usually on subject or object IDs.
  Star-shaped joins (same subject) and path joins (object = next subject) are
  the typical shapes [21][22].
- **Point lookups** in the term dictionary, to translate constants to IDs
  before evaluation and IDs back to strings for results.
- **Cardinality estimation** for join ordering: counts per prefix, or
  per characteristic set [12].

Writes are bulk loads (most data) plus small update transactions. Each logical
quad write becomes several physical writes, one per index permutation.

Values in the index structures are usually empty: the key is the whole
record. This is a sorted-set workload more than a key-value workload, and it
matters for the engine choice (section 4).

---

## 2. Index layouts

### 2.1 Exhaustive permutations

**Hexastore** [1] stores all six orderings of (S, P, O) as nested sorted
vectors (S → P → list of O, etc.). Any triple pattern is answered by one
index, and every pairwise join can be a merge join. Cost: up to 5x the space
of a single index, which the paper accepts for in-memory use.

**RDF-3X** [2][3] also keeps the six permutations, as clustered B+trees over
ID triples, plus six binary projections (SP, PS, ...) and three unary
projections (S, P, O) with counts. Leaf pages are compressed with delta
encoding of consecutive triples and a byte-level variable-length code, chosen
over bit-level schemes because decompression speed dominated. With this
compression the six full indexes take less space than the raw data. The
aggregated indexes give exact counts for join ordering. A later paper adds
sideways information passing (runtime filters between merge joins) [4] and
an update path with differential indexes merged in the background [3].

**Oxigraph** (current `main`, read from source [30]) uses 9 quad
orderings in separate RocksDB column families: `spog`, `posg`, `ospg`,
`gspo`, `gpos`, `gosp` for named graphs and `dspo`, `dpos`, `dosp` for the
default graph, plus `id2str` (term dictionary) and `graphs`. Index values are
empty. A default-graph quad costs 3 index writes, a named-graph quad 6, plus
dictionary writes for terms that are not inlined. The column family options
set a 17-byte fixed prefix extractor (type byte + 16-byte ID) on most index
families for prefix bloom filters, `optimize_for_point_lookup` on `id2str`,
LZ4 compression and an index block restart interval of 16 [31].

### 2.2 Reduced permutation sets

Many systems keep three triple orderings, which cover all 8 triple patterns
with one prefix scan each (SPO covers S, SP, SPO; POS covers P, PO; OSP
covers O, OS):

| System            | Triple indexes      | Quad / named-graph handling                            |
|-------------------|---------------------|--------------------------------------------------------|
| Jena TDB / TDB2   | SPO, POS, OSP       | GSPO, GPOS, GOSP, SPOG, POSG, OSPG [24]               |
| Blazegraph        | SPO, POS, OSP       | 6 quad orders in quads mode [25]                      |
| RDF4J NativeStore | configurable, default `spoc,posc` | context is the 4th key element [26]     |
| Apache Rya        | SPO, PO, OSP tables on Accumulo [16] | context in the column family        |
| QLever            | PSO, POS by default; all 6 optional [7] | no quad indexes in early versions  |
| Virtuoso 7        | PSOG, POGS full; SP, OP, GS partial [9] | G is part of every key              |

Virtuoso's choice is notable: two full indexes plus three "distinct
projection" indexes that only store the distinct key prefixes. Queries with
an unbound predicate are rare in practice, and the partial indexes resolve
them by enumerating predicates first [9].

### 2.3 Property tables, vertical partitioning, emergent schema

- **Vertical partitioning** [10]: one two-column (S, O) table per predicate
  in a column store. Fast for predicate-bound patterns, poor for unbound
  predicates and for data sets with thousands of predicates. [11] reproduced
  the experiments and showed a triple table with the right sort orders in the
  same column store was competitive, i.e. the gain came largely from the
  column store, not the partitioning.
- **Characteristic sets** [12]: group subjects by the set of predicates they
  have. Used first for cardinality estimation of star joins, later for
  physical layout.
- **Emergent schema** [13][14]: detect a relational schema from the data
  (most subjects fall into a small number of characteristic sets), store
  those as real tables, keep the rest as triples. Reported large speed-ups on
  star queries in MonetDB/Virtuoso prototypes.

### 2.4 Bit matrices and compact structures

- **BitMat** [5]: S×O bit matrices per predicate, compressed with run-length
  encoding, joins evaluated by bit operations before materialising results.
- **TripleBit** [6]: a single compact triple matrix with ID-chunk storage and
  two auxiliary index structures instead of six permutations; reports a
  smaller footprint than RDF-3X with comparable or better query times.
- **HDT** [17]: read-only binary format. Front-coded dictionary sections plus
  bitmap triples in SPO order; additional orderings (HDT-FoQ) are built on
  load. Used by Linked Data Fragments servers and by TerminusDB (in a
  Rust reimplementation with succinct data structures) [33].
- **k2-triples** [18]: one k²-tree (compressed adjacency matrix) per
  predicate.
- **Ring** [19]: stores the triples once as a cyclic BWT-like structure that
  supports all orderings and leapfrog-style worst-case-optimal joins, in
  space close to a single compressed copy.

These structures beat B+trees and LSM trees on space and often on read
speed, but are immutable or expensive to update. Updatable systems that use
them pair an immutable base with a delta store (same idea as RDF-3X
differential indexes, or an LSM tree).

### 2.5 Distributed key-value layouts

- **Rya** [16] on Accumulo, **CumulusRDF** [15] on Cassandra and **H2RDF+**
  [34] on HBase map triple permutations to row keys of a distributed sorted
  KV store. Evaluation surveys [22][23] found these systems competitive for
  selective lookups and slow for large joins, because joins happen outside
  the storage layer with network transfer.
- **Trinity.RDF** [20] stores the graph natively (adjacency lists per node)
  in a distributed in-memory KV store and replaces joins with graph
  exploration.
- **Wukong** [35] uses RDMA and a distributed in-memory KV store with keys of
  the form (vertex, predicate, direction).

---

## 3. Term dictionary and key encoding

Every system above replaces RDF terms with fixed-size IDs in the indexes.
Differences:

- **Sequential IDs** (RDF-3X, Jena TDB, Blazegraph, QLever): dense integers,
  smallest keys, good compression of sorted runs, ID order can follow string
  order (QLever builds the vocabulary sorted, so ID comparisons implement
  some `ORDER BY` and range filters). Requires a string → ID lookup on every
  insert and a global counter, which serialises loading.
- **Hash IDs** (Oxigraph: 128-bit SipHash-2-4 of the string [30]): no
  lookup needed to compute an ID, so writes are blind and parallel loading is
  simple. Costs: 16-byte keys per term instead of 4 or 8, random ID order
  (poor locality between terms inserted together), and no order-preserving
  comparisons on IDs.
- **Inlining**: Oxigraph stores booleans, numeric and date/time literals,
  and strings up to 16 bytes directly in the key with a 1-byte type tag;
  only longer strings go to `id2str` [30]. Blazegraph and Virtuoso inline
  numeric values too [25][9]. Inlining removes dictionary lookups for
  filters on numbers and dates and lets range filters run as index range
  scans when the type tag and value encoding are order-preserving.
- **Dictionary compression**: front coding, Re-Pair, and FM-index based
  string dictionaries are compared in [27]. Front coding over sorted
  buckets is the usual trade-off (HDT, QLever).

Oxigraph key size: a term is 1 byte tag + up to 32 bytes
(`WRITTEN_TERM_MAX_SIZE` = 33 [30]), so a quad key is up to 132 bytes and
typically 4 × 17 = 68 bytes for IRIs. Consecutive keys in an index share long
prefixes, so prefix (delta) encoding inside storage blocks matters more here
than in a dense-integer layout.

---

## 4. Persistence engines

### 4.1 Theory: read, update and memory amplification

The RUM conjecture [40] states that an access method cannot be optimal in
read cost, update cost and memory (space) overhead at the same time. The
families below are different points in that space.

### 4.2 LSM trees

Writes go to a memtable and a write-ahead log, then are flushed as immutable
sorted files (SSTables) that are merged by background compaction [41].

Published characteristics:

- Sequential writes and high ingest throughput; batched writes across
  several column families are cheap. Good fit for 3 to 9 index writes per
  quad.
- Write amplification from compaction (leveled compaction: roughly 10x per
  level-size ratio, total tens of times the user bytes). [42] (Monkey)
  tunes bloom filter memory per level; [43] (Dostoevsky) mixes tiering and
  leveling to lower write cost; [44] (PebblesDB) uses fragmented LSM trees to
  avoid rewriting data; [45] (WiscKey) separates large values from keys,
  irrelevant here because index values are empty.
- Range scans must merge all levels; bloom filters help point lookups and
  prefix lookups only. Scan-heavy SPARQL queries pay this merge cost.
- RocksDB papers [46][47] report the design decisions made at Facebook:
  space amplification reduction via dynamic level sizing, prefix bloom
  filters, range deletes, external SST ingestion for bulk load, and the
  importance of block cache and compression tuning.
- Features Oxigraph uses: column families with atomic cross-family write
  batches, snapshots, `WriteBatchWithIndex` for read-your-writes
  transactions, prefix extractors, `DeleteRange`, SST file ingestion for the
  bulk loader [31].

RDF systems on LSM storage: Oxigraph (RocksDB), Stardog 7+ (storage engine
on RocksDB, announced 2019 [36]), Rya/CumulusRDF/H2RDF (distributed LSM
stores), Apache Jena has no LSM back end in the main distribution.

### 4.3 B+trees, in-place update with buffer pool

Classic page-based B+tree with WAL and a buffer manager. Used by RDF-3X,
Jena TDB1, RDF4J NativeStore, Blazegraph (its own B+tree with a journal),
MillenniumDB [8], GraphDB.

- One random page write per insert per index (amortised by the buffer pool),
  so random inserts across many permutations are slower than in an LSM tree.
- Range scans read contiguous leaves of one structure: no merge across
  levels, predictable latency.
- Leaf compression is easy when leaves are rewritten as a unit (RDF-3X
  delta encoding).
- Modern buffer managers (LeanStore [48], Umbra [49]) close most of the gap
  to in-memory structures when the working set fits in RAM and use
  variable-size pages.

### 4.4 Copy-on-write B+trees and mmap

LMDB [37], Jena TDB2 [24], redb [38]: modified pages are written to new
locations, and a new root is published at commit. Readers see a consistent
snapshot without locks; there is one writer at a time.

- Reads are as fast as an in-place B+tree; LMDB reads via `mmap` are
  zero-copy.
- Each commit rewrites the path from leaf to root for every modified page,
  so small transactions on 9 indexes have high write amplification. Bulk
  loads in key order (LMDB `MDB_APPEND`) avoid this.
- Old pages are reclaimed only when no reader uses them; long-running read
  transactions grow the file. TDB2 needs explicit compaction for the same
  reason [24].
- RDF4J added an LMDB-based store (`LmdbStore`, RDF4J 4.0, 2022) and
  reports it as faster than its NativeStore for most workloads [26].
- Using `mmap` as the buffer manager has known problems: no control over
  eviction, I/O stalls on page faults inside transactions, TLB shootdowns
  and error handling via signals [39]. LMDB mitigates some of these with its
  single-writer design but does not avoid them.

### 4.5 Bε-trees and other write-optimised trees

Bε-trees put message buffers in internal nodes and get LSM-like insert
costs with B-tree-like queries [50]. Used in TokuDB/PerconaFT and BetrFS
[51]. No RDF store in production use is based on them as far as this survey
found. The Bw-tree [52] (latch-free, delta records, log-structured store) is
used in Microsoft products but not in published RDF systems.

### 4.6 Compressed immutable indexes with delta stores

QLever [7] builds compressed, block-wise permutations at index time and
answers queries from them; updates are a later addition handled by a
separate delta structure. HDT [17] is read-only. These designs have the best
space and scan speed in published Wikidata-scale comparisons [28][29] but
need a rebuild or merge step for updates. An LSM tree is the general form of
this design, with fewer assumptions about data.

### 4.7 Column stores and relational back ends

Virtuoso 7 stores RDF quads in column-wise compressed tables with vectored
execution [9]. Vertical partitioning experiments used C-Store and MonetDB
[10][11]. Relational mappings of RDF onto row stores (Jena SDB, early Sesame
RDBMS store, Oracle) are surveyed in [21]; the general finding was that
self-joins on a single triple table defeat row-store optimisers unless the
store adds RDF-specific statistics. Column stores do better because the
three or four columns compress well and scans are vectorised.

### 4.8 Summary

| Engine family            | Bulk load | Small updates | Range scan | Point lookup | Space   | Snapshots           |
|--------------------------|-----------|---------------|------------|--------------|---------|---------------------|
| LSM (RocksDB, fjall)     | very good (SST ingest) | good | medium (multi-level merge) | good with bloom | good (compression) | cheap |
| In-place B+tree          | good if sorted | medium   | very good  | very good    | medium  | needs MVCC layer    |
| CoW B+tree (LMDB, redb)  | good if appended | poor for many small txns | very good | very good | medium, file growth | free (by design) |
| Compressed immutable     | best      | needs delta store | best   | good         | best    | free for base       |
| Column store             | good      | medium        | best for large scans | poor | very good | depends     |

"Good/medium/poor" is a qualitative reading of the cited papers and
documentation, not a measurement on one machine.

---

## 5. Join processing and its storage needs

- **Merge joins** need inputs sorted on the join variable. With all
  permutations available, most joins in RDF-3X are merge joins [2]. Oxigraph
  currently uses nested-loop and hash joins over index scans.
- **Worst-case optimal joins** (WCOJ): [53] proves bounds; Leapfrog
  Triejoin [54] implements them over sorted tries with `seek(key)` on every
  level. For cyclic patterns (triangles, cliques) WCOJ is asymptotically
  better than pairwise joins. Applied to SPARQL in [55] (Hogan et al.,
  using Jena with extra indexes), MillenniumDB [8], Ring [19], and
  EmptyHeaded [56] for graphs. [57] integrates WCOJ into a relational
  system (Umbra) using hash tries built at query time, avoiding the need for
  every permutation on disk.
- **Storage requirement for LFTJ**: an iterator with `seek(prefix ‖ value)`
  that is cheap when the target is close to the current position. B+tree
  and LSM iterators both support seek; LSM seek touches every level, so it
  is more expensive per call. Galloping inside a decoded block helps both.
- **Sideways information passing** [4]: pass min/max or bitmaps of join
  keys from one scan to another so the second scan can skip blocks. Requires
  that the storage iterator can skip efficiently, ideally with per-block
  min/max keys exposed.

---

## 6. Benchmarks and comparisons

Benchmarks:

- LUBM [58]: synthetic university data, inference-oriented, simple shapes.
- BSBM [59]: e-commerce, explore and update mixes; used in Oxigraph's own
  benchmark scripts.
- SP²Bench [60]: DBLP-like, includes `OPTIONAL`, `FILTER`, unbound
  predicates.
- WatDiv [61]: query templates by shape (linear, star, snowflake, complex),
  designed to expose systems tuned for one shape.
- DBpedia SPARQL Benchmark [62] and FEASIBLE [63]: queries mined from real
  logs.
- LDBC Semantic Publishing Benchmark [64]: mixed read/update workload.
- Wikidata: real data (over 15 billion triples in 2024) and real query logs.

Published comparisons relevant to storage:

- [28] (Malyshev et al., ISWC 2018) compared Blazegraph, Virtuoso,
  PostgreSQL and Neo4j on Wikidata; Virtuoso and Blazegraph were fastest,
  and the authors note storage reification overhead for statements.
- The Wikidata Query Service backend evaluation (Wikimedia, 2022 onwards)
  [29] examined replacements for Blazegraph, including QLever, Virtuoso,
  MillenniumDB and Jena, citing Blazegraph's lack of maintenance and its
  update throughput problems.
- QLever's published Wikidata comparisons [7][29] show it far faster on
  large-result queries than Blazegraph and Virtuoso, with a much smaller
  index, at the cost of slow updates.
- Survey and evaluation papers [22][23] (distributed SPARQL) and [21]
  (single-node RDF stores) conclude that no system dominates on all query
  shapes; layout determines which shapes are fast.

Gap: there is no published, controlled study that holds the SPARQL engine
constant and swaps only the key-value engine (RocksDB vs LMDB vs B+tree vs
custom LSM) for an RDF workload. The closest are RDF4J's NativeStore vs
LmdbStore numbers [26], which also change the record format. Running BSBM
and WatDiv on Oxigraph with RocksDB vs `oxnative` would fill this gap.

---

## 7. Consequences for `oxnative`

The contract in `README.md` and `src/lib.rs` fixes the data model:
Oxigraph's column families, key encoding and 9 permutations stay as they
are. `oxnative` decides only how sorted key sets are stored, cached, made
durable, snapshotted and bulk-loaded.

### 7.1 Requirements that follow from the contract

| Contract item                                   | Engine requirement                                   |
|-------------------------------------------------|------------------------------------------------------|
| Fixed column families, atomic batches across them | one WAL/commit point for all families              |
| `Reader<'static>` snapshots, `Clone + Send + Sync` | MVCC with reference-counted versions, no thread-bound read txns |
| Prefix iteration, ordered                        | sorted structure; seek to prefix; upper bound check |
| Read-your-writes transactions                    | overlay of pending writes on a snapshot (RocksDB uses `WriteBatchWithIndex`) |
| `remove_range`                                   | range tombstones (LSM) or range delete in tree       |
| `new_sst_file` + `insert_stt_files`              | build sorted runs outside the DB and link them atomically into several families |
| `backup`, read-only open                         | immutable files (hard links) or consistent copy      |

These map almost one-to-one onto LSM features (sorted runs, range
tombstones, file ingestion, cheap snapshots, hard-link checkpoints).
A copy-on-write B+tree satisfies snapshots and atomicity naturally but has no
equivalent of external sorted-file ingestion (it must append-insert) and no
cheap range tombstone.

### 7.2 Workload-specific opportunities

1. **Key-only families.** All index families store empty values. A block
   format that stores keys only, with prefix compression against the
   previous key, avoids per-entry value length fields.
2. **Long shared prefixes.** 17-byte term IDs repeat across consecutive keys
   (`spog` sorted by S then P). Prefix compression with restart points (as
   in LevelDB/RocksDB blocks [46]) or per-column delta encoding (RDF-3X [2])
   applies directly.
3. **Prefix bloom filters.** Oxigraph already declares 17-byte prefixes per
   family; a native engine can build per-run bloom filters on the first term
   and skip runs on bound-subject or bound-predicate scans.
4. **Per-block min/max keys** in the run index allow skipping for
   sideways information passing and for leapfrog `seek` [4][54].
5. **Point-lookup family.** `id2str` is a hash-keyed point-lookup map with
   `unordered_writes` in RocksDB; it can use a different structure
   (hash-partitioned runs, or larger blocks with a bloom filter per run)
   from the index families.
6. **Bulk load.** Oxigraph's bulk loader sorts quads in memory and writes
   SST files per family. A native format can write the final on-disk run
   directly, with no compaction afterwards.
7. **Write amplification budget.** With 6 index writes per named-graph quad,
   compaction write amplification multiplies 6x. Tiered or hybrid compaction
   [43] is worth evaluating for update-heavy use; leveled compaction is
   better for read-heavy use.

### 7.3 Implementation options

**A. Wrap an existing Rust engine.** Fastest way to make the acceptance
tests pass and get a baseline.

- `fjall` [65]: pure-Rust LSM tree with keyspaces/partitions, cross-partition
  atomic write batches, snapshots, and block-level compression. Closest to
  RocksDB semantics. Check support for range deletes and external run
  ingestion in the version used; emulate if missing.
- `redb` [38]: pure-Rust copy-on-write B-tree, single writer, MVCC readers,
  stable file format since 1.0. Simple and robust; expect slower bulk loads
  and small-transaction writes over 9 tables.
- `heed` (LMDB bindings) [37]: not pure Rust (C library), so it conflicts
  with the crate's purpose, but useful as a comparison point.
- `sled`: long-running rewrite, not recommended as a dependency for a
  storage format that must stay stable.

**B. Purpose-built LSM for sorted key sets.** Memtable (skip list or B-tree
map), WAL shared by all families, immutable runs with key-only
prefix-compressed blocks, run-level bloom filters on the declared prefix,
range tombstones, leveled or tiered compaction, manifest for atomic file
sets (gives `insert_stt_files` and `backup` via hard links). More work, but
every item in 7.2 becomes possible.

**C. Immutable compressed base + delta.** QLever/HDT-style permutations
built at bulk load, with a small LSM or B-tree delta for updates and periodic
merge. Best space and scan speed for read-mostly data; essentially a two-level
LSM with a specialised bottom level. Can be reached incrementally from B by
specialising the last level's file format.

Suggested sequence: implement A with `fjall` (or `redb`) to pass the
fork's tests; build a benchmark harness with BSBM explore/update and WatDiv
on Oxigraph comparing RocksDB vs `oxnative`; then move to B, replacing parts
where the profile shows cost (block format and bloom filters first, then
compaction policy); keep C as the optimisation for the bottom level.

### 7.4 Things to measure

- Bulk load time and final size for a fixed data set (e.g. BSBM 100M,
  WatDiv 100M, a Wikidata subset).
- Query latency per WatDiv shape class (L, S, F, C) and BSBM explore mix.
- Update throughput for small transactions (BSBM update mix), with p99.
- Write amplification (bytes written to disk / bytes of keys inserted) and
  space amplification after updates and deletes.
- Cold-cache vs warm-cache results; the published systems differ most on
  cold reads.

---

## References

Index layouts and RDF stores

1. C. Weiss, P. Karras, A. Bernstein. *Hexastore: Sextuple Indexing for Semantic Web Data Management.* PVLDB 1(1), 2008.
2. T. Neumann, G. Weikum. *RDF-3X: a RISC-style Engine for RDF.* PVLDB 1(1), 2008.
3. T. Neumann, G. Weikum. *The RDF-3X Engine for Scalable Management of RDF Data.* VLDB Journal 19(1), 2010.
4. T. Neumann, G. Weikum. *Scalable Join Processing on Very Large RDF Graphs.* SIGMOD 2009.
5. M. Atre, V. Chaoji, M. J. Zaki, J. A. Hendler. *Matrix "Bit" loaded: a scalable lightweight join query processor for RDF data.* WWW 2010.
6. P. Yuan, P. Liu, B. Wu, H. Jin, W. Zhang, L. Liu. *TripleBit: a Fast and Compact System for Large Scale RDF Data.* PVLDB 6(7), 2013.
7. H. Bast, B. Buchhold. *QLever: A Query Engine for Efficient SPARQL+Text Search.* CIKM 2017. Source: https://github.com/ad-freiburg/qlever
8. D. Vrgoč et al. *MillenniumDB: An Open-Source Graph Database System.* Data Intelligence 5(3), 2023. Source: https://github.com/MillenniumDB/MillenniumDB
9. O. Erling, I. Mikhailov. *RDF Support in the Virtuoso DBMS.* In Networked Knowledge – Networked Media, Springer, 2009; Virtuoso documentation, "RDF Index Scheme" and column store sections: https://docs.openlinksw.com/virtuoso/
10. D. J. Abadi, A. Marcus, S. R. Madden, K. Hollenbach. *Scalable Semantic Web Data Management Using Vertical Partitioning.* VLDB 2007.
11. L. Sidirourgos, R. Goncalves, M. Kersten, N. Nes, S. Manegold. *Column-Store Support for RDF Data Management: not all swans are white.* PVLDB 1(2), 2008.
12. T. Neumann, G. Moerkotte. *Characteristic Sets: Accurate Cardinality Estimation for RDF Queries with Multiple Joins.* ICDE 2011.
13. M.-D. Pham, P. Boncz. *Exploiting Emergent Schemas to Make RDF Systems More Efficient.* ISWC 2016.
14. M.-D. Pham, L. Passing, O. Erling, P. Boncz. *Deriving an Emergent Relational Schema from RDF Data.* WWW 2015.
15. G. Ladwig, A. Harth. *CumulusRDF: Linked Data Management on Nested Key-Value Stores.* SSWS 2011.
16. R. Punnoose, A. Crainiceanu, D. Rapp. *Rya: A Scalable RDF Triple Store for the Clouds.* Cloud-I workshop, VLDB 2012.
17. J. D. Fernández, M. A. Martínez-Prieto, C. Gutiérrez, A. Polleres, M. Arias. *Binary RDF Representation for Publication and Exchange (HDT).* Journal of Web Semantics 19, 2013.
18. S. Álvarez-García, N. Brisaboa, J. D. Fernández, M. A. Martínez-Prieto, G. Navarro. *Compressed vertical partitioning for efficient RDF management.* Knowledge and Information Systems 44(2), 2015.
19. D. Arroyuelo, A. Hogan, G. Navarro, J. Reutter, J. Rojas-Ledesma, A. Soto. *Worst-Case Optimal Graph Joins in Almost No Space.* SIGMOD 2021.
20. K. Zeng, J. Yang, H. Wang, B. Shao, Z. Wang. *A Distributed Graph Engine for Web Scale RDF Data (Trinity.RDF).* PVLDB 6(4), 2013.

Surveys and evaluations

21. W. Ali, M. Saleem, B. Yao, A. Hogan, A.-C. Ngonga Ngomo. *A Survey of RDF Stores & SPARQL Engines for Querying Knowledge Graphs.* VLDB Journal 31, 2022. Also: M. T. Özsu, *A survey of RDF data management systems*, Frontiers of Computer Science 10(3), 2016; S. Sakr, G. Al-Naymat, *Relational processing of RDF queries: a survey*, SIGMOD Record 38(4), 2010.
22. I. Abdelaziz, R. Harbi, Z. Khayyat, P. Kalnis. *A Survey and Experimental Comparison of Distributed SPARQL Engines for Very Large RDF Data.* PVLDB 10(13), 2017.
23. Z. Kaoudi, I. Manolescu. *RDF in the clouds: a survey.* VLDB Journal 24(1), 2015.

Production system documentation and source

24. Apache Jena. *TDB* and *TDB2* documentation (indexes, node table, TDB2 copy-on-write B+trees and compaction): https://jena.apache.org/documentation/tdb/ and https://jena.apache.org/documentation/tdb2/
25. Blazegraph (Bigdata) wiki: *Data Model / Indices / Inline values*: https://github.com/blazegraph/database/wiki
26. Eclipse RDF4J documentation: *NativeStore* (index configuration) and *LMDB Store* (since 4.0): https://rdf4j.org/documentation/programming/repository/
27. M. A. Martínez-Prieto, N. Brisaboa, R. Cánovas, F. Claude, G. Navarro. *Practical compressed string dictionaries.* Information Systems 56, 2016.
28. S. Malyshev, M. Krötzsch, L. González, J. Gonsior, A. Bielefeldt. *Getting the Most out of Wikidata: Semantic Technology Usage in Wikipedia's Knowledge Graph.* ISWC 2018.
29. Wikimedia. *Wikidata Query Service backend update / Blazegraph alternatives* evaluation pages on wikitech and mediawiki.org (2022–), and QLever's Wikidata performance comparison: https://github.com/ad-freiburg/qlever/wiki
30. Oxigraph source, `lib/oxigraph/src/storage/{rocksdb.rs, numeric_encoder.rs, binary_encoder.rs, small_string.rs}`, branch `main` as of commit `28e801a` (2026): https://github.com/oxigraph/oxigraph
31. Oxigraph source, `lib/oxigraph/src/storage/rocksdb_wrapper.rs` (RocksDB options: LZ4, block format version 5, prefix extractors, `optimize_for_point_lookup`, `unordered_write`, `WriteBatchWithIndex` transactions), same commit. Relevant recent commits: `8887aa2` "RocksDB: Make sure transactions can read the strings they just inserted", `ecfdf53` "RocksDB clear_all_named_graphs: fixes batching logic", `4e6d2e5` "BlankNode: remove inlined variant", `3ce3f6e` "Store: make Transaction and BulkLoader Send and Sync".
32. Oxigraph source, `StorageBulkLoader` in `lib/oxigraph/src/storage/rocksdb.rs` (in-memory sort of encoded quads, one SST file per column family, atomic ingestion), same commit.
33. TerminusDB storage layer (`terminusdb-store`, Rust, succinct structures): https://github.com/terminusdb/terminusdb-store
34. N. Papailiou, I. Konstantinou, D. Tsoumakos, P. Karras, N. Koziris. *H2RDF+: High-performance distributed joins over large-scale RDF graphs.* IEEE Big Data 2013.
35. J. Shi, Y. Yao, R. Chen, H. Chen, F. Li. *Fast and Concurrent RDF Queries with RDMA-Based Distributed Graph Exploration (Wukong).* OSDI 2016.
36. Stardog. *Stardog 7* release notes and blog posts on the "Mastiff" storage engine built on RocksDB (2019): https://docs.stardog.com/
37. H. Chu. *MDB: A Memory-Mapped Database and Backend for OpenLDAP.* LDAPCon 2011. LMDB: http://www.lmdb.tech/doc/ ; Rust bindings `heed`: https://github.com/meilisearch/heed
38. redb, pure-Rust embedded database (copy-on-write B-trees): https://github.com/cberner/redb (README includes the author's benchmarks against lmdb, rocksdb, sled and fjall).
39. A. Crotty, V. Leis, A. Pavlo. *Are You Sure You Want to Use MMAP in Your Database Management System?* CIDR 2022.

Storage engines

40. M. Athanassoulis, M. S. Kester, L. M. Maas, R. Stoica, S. Idreos, A. Ailamaki, M. Callaghan. *Designing Access Methods: The RUM Conjecture.* EDBT 2016.
41. P. O'Neil, E. Cheng, D. Gawlick, E. O'Neil. *The Log-Structured Merge-Tree (LSM-Tree).* Acta Informatica 33(4), 1996.
42. N. Dayan, M. Athanassoulis, S. Idreos. *Monkey: Optimal Navigable Key-Value Store.* SIGMOD 2017.
43. N. Dayan, S. Idreos. *Dostoevsky: Better Space-Time Trade-Offs for LSM-Tree Based Key-Value Stores via Adaptive Removal of Superfluous Merging.* SIGMOD 2018.
44. P. Raju, R. Kadekodi, V. Chidambaram, I. Abraham. *PebblesDB: Building Key-Value Stores using Fragmented Log-Structured Merge Trees.* SOSP 2017.
45. L. Lu, T. S. Pillai, A. C. Arpaci-Dusseau, R. H. Arpaci-Dusseau. *WiscKey: Separating Keys from Values in SSD-conscious Storage.* FAST 2016.
46. S. Dong, M. Callaghan, L. Galanis, D. Borthakur, T. Savor, M. Strum. *Optimizing Space Amplification in RocksDB.* CIDR 2017.
47. S. Dong, A. Kryczka, Y. Jin, M. Stumm. *RocksDB: Evolution of Development Priorities in a Key-value Store Serving Large-scale Applications.* ACM Transactions on Storage 17(4), 2021.
48. V. Leis, M. Haubenschild, A. Kemper, T. Neumann. *LeanStore: In-Memory Data Management Beyond Main Memory.* ICDE 2018.
49. T. Neumann, M. Freitag. *Umbra: A Disk-Based System with In-Memory Performance.* CIDR 2020.
50. M. A. Bender, M. Farach-Colton, W. Jannen, R. Johnson, B. C. Kuszmaul, D. E. Porter, J. Yuan, Y. Zhan. *An Introduction to Bε-trees and Write-Optimization.* ;login: 40(5), 2015. Original structure: G. S. Brodal, R. Fagerberg, *Lower Bounds for External Memory Dictionaries*, SODA 2003.
51. W. Jannen et al. *BetrFS: A Right-Optimized Write-Optimized File System.* FAST 2015.
52. J. Levandoski, D. Lomet, S. Sengupta. *The Bw-Tree: A B-tree for New Hardware Platforms.* ICDE 2013.

Joins

53. H. Q. Ngo, E. Porat, C. Ré, A. Rudra. *Worst-case Optimal Join Algorithms.* PODS 2012.
54. T. L. Veldhuizen. *Leapfrog Triejoin: A Simple, Worst-Case Optimal Join Algorithm.* ICDT 2014.
55. A. Hogan, C. Riveros, C. Rojas, A. Soto. *A Worst-Case Optimal Join Algorithm for SPARQL.* ISWC 2019.
56. C. R. Aberger, S. Tu, K. Olukotun, C. Ré. *EmptyHeaded: A Relational Engine for Graph Processing.* SIGMOD 2016.
57. M. Freitag, M. Bandle, T. Schmidt, A. Kemper, T. Neumann. *Adopting Worst-Case Optimal Joins in Relational Database Systems.* PVLDB 13(11), 2020.

Benchmarks

58. Y. Guo, Z. Pan, J. Heflin. *LUBM: A Benchmark for OWL Knowledge Base Systems.* Journal of Web Semantics 3(2–3), 2005.
59. C. Bizer, A. Schultz. *The Berlin SPARQL Benchmark.* International Journal on Semantic Web and Information Systems 5(2), 2009.
60. M. Schmidt, T. Hornung, G. Lausen, C. Pinkel. *SP²Bench: A SPARQL Performance Benchmark.* ICDE 2009.
61. G. Aluç, O. Hartig, M. T. Özsu, K. Daudjee. *Diversified Stress Testing of RDF Data Management Systems (WatDiv).* ISWC 2014.
62. M. Morsey, J. Lehmann, S. Auer, A.-C. Ngonga Ngomo. *DBpedia SPARQL Benchmark – Performance Assessment with Real Queries on Real Data.* ISWC 2011.
63. M. Saleem, Q. Mehmood, A.-C. Ngonga Ngomo. *FEASIBLE: A Feature-Based SPARQL Benchmark Generation Framework.* ISWC 2015.
64. LDBC. *Semantic Publishing Benchmark* specification: https://ldbcouncil.org/benchmarks/spb/

Rust engines

65. fjall, LSM-tree based storage engine in Rust: https://github.com/fjall-rs/fjall (and `lsm-tree` crate).
