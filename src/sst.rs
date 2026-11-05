//! Sorted files written by bulk loads, and their merge into a keyspace.
//!
//! The format is private to this crate: a magic header, then records of
//! `[u32 LE key length][key][u32 LE value length][value]` in strictly
//! increasing key order.

use crate::error::KvError;
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

const MAGIC: &[u8; 8] = b"OXNSST1\n";

pub struct Writer {
    file: BufWriter<File>,
    path: PathBuf,
    last_key: Option<Vec<u8>>,
}

impl Writer {
    pub fn create(path: PathBuf) -> Result<Self, KvError> {
        // create_new: never overwrite a file of a concurrent bulk load
        let mut file = BufWriter::new(File::create_new(&path)?);
        file.write_all(MAGIC)?;
        Ok(Self {
            file,
            path,
            last_key: None,
        })
    }

    pub fn insert(&mut self, key: &[u8], value: &[u8]) -> Result<(), KvError> {
        if self.last_key.as_deref().is_some_and(|last| last >= key) {
            return Err(KvError::other(
                "keys must be inserted in strictly increasing order in a bulk-load file",
            ));
        }
        write_chunk(&mut self.file, key)?;
        write_chunk(&mut self.file, value)?;
        self.last_key = Some(key.to_vec());
        Ok(())
    }

    pub fn finish(self) -> Result<PathBuf, KvError> {
        let file = self
            .file
            .into_inner()
            .map_err(io::IntoInnerError::into_error)?;
        file.sync_all()?;
        Ok(self.path)
    }
}

fn write_chunk(file: &mut impl Write, chunk: &[u8]) -> io::Result<()> {
    let len = u32::try_from(chunk.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "key or value too large"))?;
    file.write_all(&len.to_le_bytes())?;
    file.write_all(chunk)
}

struct Reader {
    file: BufReader<File>,
}

impl Reader {
    fn open(path: &Path) -> Result<Self, KvError> {
        let mut file = BufReader::new(File::open(path)?);
        let mut magic = [0; MAGIC.len()];
        file.read_exact(&mut magic)?;
        if &magic != MAGIC {
            return Err(KvError::corruption(format!(
                "{} is not an oxnative bulk-load file",
                path.display()
            )));
        }
        Ok(Self { file })
    }

    fn next(&mut self) -> Result<Option<(Vec<u8>, Vec<u8>)>, KvError> {
        let mut len = [0; 4];
        // A clean end of file can only happen before a record
        match self.file.read_exact(&mut len) {
            Ok(()) => (),
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e.into()),
        }
        let key = self.read_chunk(len)?;
        self.file.read_exact(&mut len)?;
        let value = self.read_chunk(len)?;
        Ok(Some((key, value)))
    }

    fn read_chunk(&mut self, len: [u8; 4]) -> Result<Vec<u8>, KvError> {
        let len = usize::try_from(u32::from_le_bytes(len))
            .map_err(|_| KvError::corruption("record too large for this platform"))?;
        let mut chunk = vec![0; len];
        self.file.read_exact(&mut chunk)?;
        Ok(chunk)
    }
}

/// Ingests the union of the given files into `keyspace`.
///
/// Files are individually sorted but may overlap: they are merged and, for keys
/// present in several files, the value of the last file wins. Ingested data
/// gets a newer sequence number than everything already in the keyspace, so it
/// overrides existing values and removals.
pub fn ingest(keyspace: &fjall::Keyspace, paths: &[PathBuf]) -> Result<(), KvError> {
    let mut readers = paths
        .iter()
        .map(|p| Reader::open(p))
        .collect::<Result<Vec<_>, _>>()?;
    // Min-heap on (key, reversed file index): for equal keys the last file pops
    // first, and the later duplicates are skipped. The current value of each
    // file waits in `values`, by file index.
    let mut heap = BinaryHeap::with_capacity(readers.len());
    let mut values = Vec::with_capacity(readers.len());
    for (i, reader) in readers.iter_mut().enumerate() {
        let entry = reader.next()?;
        values.push(entry.map(|(key, value)| {
            heap.push(Reverse((key, Reverse(i))));
            value
        }));
    }

    let mut ingestion = keyspace.start_ingestion()?;
    let mut last_key: Option<Vec<u8>> = None;
    while let Some(Reverse((key, Reverse(i)))) = heap.pop() {
        let (Some(reader), Some(slot)) = (readers.get_mut(i), values.get_mut(i)) else {
            return Err(KvError::other(
                "internal error: bulk-load merge out of sync",
            ));
        };
        let Some(value) = slot.take() else {
            return Err(KvError::other(
                "internal error: bulk-load merge out of sync",
            ));
        };
        if last_key.as_deref() != Some(key.as_slice()) {
            ingestion.write(key.as_slice(), value)?;
            last_key = Some(key);
        }
        if let Some((next_key, next_value)) = reader.next()? {
            heap.push(Reverse((next_key, Reverse(i))));
            *slot = Some(next_value);
        }
    }
    ingestion.finish()?;
    Ok(())
}
