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

/// A trait for types that can be used as nested collections.
pub trait DurableCollection {
    /// Creates a new instance of the collection from a database handle
    /// and a pre-determined, unique key prefix.
    /// 
    /// This is the key method that allows `DurableMap` to instantiate
    /// a nested collection handle.
    fn from_prefix(db: Db, prefix: Vec<u8>) -> Self;
}

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
    
    /// Get a new unique collection ID for nested collections
    pub fn new_collection_id(&self) -> Result<u64> {
        let key = b"__global_meta:next_collection_id";
        
        // Get current value
        let current_bytes = self.rocks().get(key)?;
        let current_id = match current_bytes {
            Some(bytes) => {
                if bytes.len() != 8 {
                    return Err(DurableError::Corruption("Invalid collection ID bytes size".into()));
                }
                let id_bytes: [u8; 8] = bytes[..8].try_into()
                    .map_err(|_| DurableError::Corruption("Invalid collection ID bytes".into()))?;
                u64::from_le_bytes(id_bytes)
            }
            None => 0,
        };
        
        let next_id = current_id + 1;
        
        // Try to atomically update - use compare-and-swap semantics
        let mut batch = WriteBatch::default();
        batch.put(key, &next_id.to_le_bytes());
        
        // For now, just write it directly. In a real implementation,
        // we'd want proper compare-and-swap to handle concurrent access
        self.rocks().write(batch)?;
        self.rocks().flush_wal(true)?;
        
        Ok(current_id)
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
