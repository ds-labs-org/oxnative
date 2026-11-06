//! Private point-in-time copy of a database, used by read-only handles.
//!
//! fjall holds an exclusive lock on its directory and has no read-only mode.
//! A read-only handle therefore opens a private copy: the immutable table files
//! (`tables/` directories) are hard-linked when possible, everything else
//! (journals, manifests, small metadata files and fjall's empty lock file, which
//! recovery expects to exist) is copied. The copy keeps the
//! view of the moment it was made and is deleted when the handle is dropped.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Name of the directories holding immutable table files in lsm-tree.
const TABLES_FOLDER: &str = "tables";

pub struct PrivateCopy {
    path: PathBuf,
}

impl PrivateCopy {
    /// Copies `source` (a fjall directory) next to `source` if possible, so
    /// that table files can be hard-linked, else into the system temporary
    /// directory.
    pub fn create(source: &Path, database_root: &Path) -> io::Result<Self> {
        let mut last_error = None;
        for parent in candidate_parents(database_root) {
            let path = parent.join(unique_name(database_root));
            #[expect(clippy::create_dir, reason = "the directory must be new")]
            let created = fs::create_dir(&path);
            match created {
                Ok(()) => {
                    let copy = Self { path };
                    copy_tree(source, &copy.path)?;
                    return Ok(copy);
                }
                Err(e) => last_error = Some(e),
            }
        }
        Err(last_error
            .unwrap_or_else(|| io::Error::other("no directory available for the read-only copy")))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for PrivateCopy {
    fn drop(&mut self) {
        // Best effort: a leftover copy only wastes space.
        #[expect(unused_must_use, reason = "nowhere to report the error")]
        fs::remove_dir_all(&self.path);
    }
}

fn candidate_parents(database_root: &Path) -> Vec<PathBuf> {
    let mut parents = Vec::new();
    if let Some(parent) = database_root.parent() {
        parents.push(if parent.as_os_str().is_empty() {
            PathBuf::from(".")
        } else {
            parent.to_path_buf()
        });
    }
    parents.push(std::env::temp_dir());
    parents
}

fn unique_name(database_root: &Path) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let base = database_root
        .file_name()
        .map_or_else(|| "db".into(), |n| n.to_string_lossy().into_owned());
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!(
        ".{base}.oxnative-ro-{}-{nanos}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

fn copy_tree(source: &Path, target: &Path) -> io::Result<()> {
    let link_files = source.file_name().is_some_and(|n| n == TABLES_FOLDER);
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let from = entry.path();
        let to = target.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            #[expect(clippy::create_dir, reason = "the parent was just created")]
            fs::create_dir(&to)?;
            copy_tree(&from, &to)?;
        } else if !link_files || fs::hard_link(&from, &to).is_err() {
            fs::copy(&from, &to)?;
        }
    }
    Ok(())
}
