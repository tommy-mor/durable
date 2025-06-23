//! Durable - RocksDB-backed persistent data structures for Rust

use std::path::Path;
use std::sync::Arc;
use rocksdb::{DB as RocksDB, Options, WriteBatch};
use thiserror::Error;

pub mod vec;
pub mod map;
pub use vec::DurableVec;
pub use map::DurableMap;

/// Error types for Durable operations
#[derive(Error, Debug)]
pub enum DurableError {
    #[error("RocksDB error: {0}")]
    RocksDB(#[from] rocksdb::Error),
    
    #[error("Serialization error: {0}")]
    Serialization(#[from] bincode::Error),
    
    #[error("Key not found")]
    KeyNotFound,
    
    #[error("Collection not found: {0}")]
    CollectionNotFound(String),
    
    #[error("Data corruption: {0}")]
    Corruption(String),
}

pub type Result<T> = std::result::Result<T, DurableError>;

/// The main database handle
#[derive(Clone)]
pub struct Db {
    inner: Arc<RocksDB>,
}

impl Db {
    /// Opens or creates a durable database at the given path
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let mut opts = Options::default();
        opts.create_if_missing(true);
        opts.create_missing_column_families(true);
        
        let db = RocksDB::open(&opts, path)?;
        Ok(Db { 
            inner: Arc::new(db),
        })
    }
    
    /// Create a new write batch for atomic operations
    pub fn batch(&self) -> Batch {
        Batch {
            db: self.clone(),
            inner: WriteBatch::default(),
        }
    }
    
    /// Get the underlying RocksDB handle (for advanced usage)
    pub(crate) fn rocks(&self) -> &RocksDB {
        &self.inner
    }
}

/// A write batch for atomic operations
pub struct Batch {
    db: Db,
    inner: WriteBatch,
}

impl Batch {
    /// Commit all operations in this batch atomically
    pub fn commit(self) -> Result<()> {
        self.db.rocks().write(self.inner)?;
        self.db.rocks().flush_wal(true)?;
        Ok(())
    }
}
