use std::error::Error as StdError;
use std::{fmt, io};

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
}

impl KvError {
    pub(crate) fn other(msg: impl Into<String>) -> Self {
        Self::Other(msg.into().into())
    }

    pub(crate) fn corruption(msg: impl Into<String>) -> Self {
        Self::Corruption(msg.into().into())
    }

    pub(crate) fn read_only(operation: &str) -> Self {
        Self::other(format!(
            "{operation} is only possible on read-write instances"
        ))
    }

    pub(crate) fn poisoned() -> Self {
        Self::other("an oxnative lock is poisoned: a thread panicked while writing")
    }
}

impl fmt::Display for KvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => e.fmt(f),
            Self::Corruption(e) | Self::Other(e) => e.fmt(f),
            Self::MissingColumnFamily(name) => write!(f, "Column family {name} does not exist"),
        }
    }
}

impl StdError for KvError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Corruption(e) | Self::Other(e) => Some(&**e),
            Self::MissingColumnFamily(_) => None,
        }
    }
}

impl From<io::Error> for KvError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<fjall::Error> for KvError {
    fn from(error: fjall::Error) -> Self {
        match error {
            fjall::Error::Io(e) | fjall::Error::Storage(fjall::LsmError::Io(e)) => Self::Io(e),
            fjall::Error::Locked => {
                Self::other("the database is already opened by another read-write instance")
            }
            fjall::Error::Poisoned | fjall::Error::KeyspaceDeleted => Self::Other(Box::new(error)),
            e => Self::Corruption(Box::new(e)),
        }
    }
}

/// Builds an error from one that cannot be moved (kept by an iterator).
pub(crate) fn from_fjall_ref(error: &fjall::Error) -> KvError {
    match error {
        fjall::Error::Io(e) | fjall::Error::Storage(fjall::LsmError::Io(e)) => {
            KvError::Io(io::Error::new(e.kind(), e.to_string()))
        }
        e => KvError::corruption(e.to_string()),
    }
}
