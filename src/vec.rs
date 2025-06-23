use crate::{Db, Result, DurableError, DurableCollection};
use rocksdb::WriteBatch;
use serde::{Serialize, Deserialize};
use std::marker::PhantomData;

/// A persistent vector backed by RocksDB
pub struct DurableVec<T> {
    db: Db,
    prefix: Vec<u8>,
    _phantom: PhantomData<T>,
}

impl<T> DurableVec<T> 
where 
    T: Serialize + for<'de> Deserialize<'de>
{
    /// Create a new DurableVec with the given name
    pub fn new(db: &Db, name: &str) -> Result<Self> {
        let prefix = format!("vec:{}", name).into_bytes();
        
        Ok(DurableVec {
            db: db.clone(),
            prefix,
            _phantom: PhantomData,
        })
    }
    
    /// Create a new DurableVec from a prefix (used for nested collections)
    pub fn from_prefix(db: Db, prefix: Vec<u8>) -> Self {
        Self {
            db,
            prefix,
            _phantom: PhantomData,
        }
    }
    
    /// Get the length of the vector
    pub fn len(&self) -> Result<usize> {
        let key = self.meta_key("len");
        match self.db.rocks().get(&key)? {
            Some(bytes) => {
                if bytes.len() != 8 {
                    return Err(DurableError::Corruption("Invalid length bytes size".into()));
                }
                let len_bytes: [u8; 8] = bytes[..8].try_into()
                    .map_err(|_| DurableError::Corruption("Invalid length bytes".into()))?;
                Ok(u64::from_le_bytes(len_bytes) as usize)
            }
            None => Ok(0),
        }
    }
    
    /// Check if the vector is empty
    pub fn is_empty(&self) -> Result<bool> {
        Ok(self.len()? == 0)
    }
    
    /// Push an element to the end of the vector
    pub fn push(&mut self, value: T) -> Result<()> {
        let len = self.len()?;
        let mut batch = WriteBatch::default();
        
        // Serialize the value
        let value_bytes = bincode::serialize(&value)?;
        
        // Write the element
        let elem_key = self.element_key(len);
        batch.put(&elem_key, &value_bytes);
        
        // Update the length
        let new_len = (len + 1) as u64;
        let len_key = self.meta_key("len");
        batch.put(&len_key, &new_len.to_le_bytes());
        
        // Commit atomically
        self.db.rocks().write(batch)?;
        self.db.rocks().flush_wal(true)?;
        
        Ok(())
    }
    
    /// Get an element at the given index
    pub fn get(&self, index: usize) -> Result<Option<T>> {
        let len = self.len()?;
        if index >= len {
            return Ok(None);
        }
        
        let key = self.element_key(index);
        match self.db.rocks().get(&key)? {
            Some(bytes) => {
                let value = bincode::deserialize(&bytes)?;
                Ok(Some(value))
            }
            None => Err(DurableError::Corruption(
                format!("Element at index {} not found but index < len", index)
            )),
        }
    }
    
    /// Clear all elements from the vector
    pub fn clear(&mut self) -> Result<()> {
        let len = self.len()?;
        let mut batch = WriteBatch::default();
        
        // Delete all elements
        for i in 0..len {
            let key = self.element_key(i);
            batch.delete(&key);
        }
        
        // Delete the length meta key
        let len_key = self.meta_key("len");
        batch.delete(&len_key);
        
        // Commit atomically
        self.db.rocks().write(batch)?;
        self.db.rocks().flush_wal(true)?;
        
        Ok(())
    }
    
    /// Create a streaming iterator over the vector
    pub fn iter(&self) -> Result<impl Iterator<Item = Result<T>> + '_> {
        let prefix = self.element_prefix();
        let iter = self.db.rocks().iterator(rocksdb::IteratorMode::From(&prefix, rocksdb::Direction::Forward));
        
        Ok(VecIterator {
            inner: iter,
            prefix,
            _phantom: PhantomData,
        })
    }
    
    /// Convert the entire vector to a Vec<T> in memory
    /// 
    /// Note: This loads the entire collection into memory. For large collections,
    /// prefer using `iter()` which streams elements.
    pub fn to_vec(&self) -> Result<Vec<T>> {
        let len = self.len()?;
        let mut result = Vec::with_capacity(len);
        
        for item in self.iter()? {
            result.push(item?);
        }
        
        Ok(result)
    }
    
    /// Push multiple elements in a single batch
    pub fn extend<I>(&mut self, iter: I) -> Result<()>
    where
        I: IntoIterator<Item = T>
    {
        let mut batch = WriteBatch::default();
        let mut len = self.len()?;
        
        for value in iter {
            let value_bytes = bincode::serialize(&value)?;
            let elem_key = self.element_key(len);
            batch.put(&elem_key, &value_bytes);
            len += 1;
        }
        
        // Update length
        let len_key = self.meta_key("len");
        batch.put(&len_key, &(len as u64).to_le_bytes());
        
        // Commit atomically
        self.db.rocks().write(batch)?;
        self.db.rocks().flush_wal(true)?;
        
        Ok(())
    }
    
    /// Remove and return the last element
    pub fn pop(&mut self) -> Result<Option<T>> {
        let len = self.len()?;
        if len == 0 {
            return Ok(None);
        }
        
        let last_idx = len - 1;
        let value = self.get(last_idx)?;
        
        let mut batch = WriteBatch::default();
        
        // Delete the last element
        let elem_key = self.element_key(last_idx);
        batch.delete(&elem_key);
        
        // Update length
        let len_key = self.meta_key("len");
        batch.put(&len_key, &(last_idx as u64).to_le_bytes());
        
        // Commit atomically
        self.db.rocks().write(batch)?;
        self.db.rocks().flush_wal(true)?;
        
        Ok(value)
    }
    
    // Helper methods
    
    fn element_key(&self, index: usize) -> Vec<u8> {
        let mut key = self.prefix.clone();
        key.push(b':');
        key.extend_from_slice(&(index as u64).to_be_bytes());
        key
    }
    
    fn meta_key(&self, meta_type: &str) -> Vec<u8> {
        let mut key = self.prefix.clone();
        key.extend_from_slice(b":__meta:");
        key.extend_from_slice(meta_type.as_bytes());
        key
    }
    
    fn element_prefix(&self) -> Vec<u8> {
        let mut prefix = self.prefix.clone();
        prefix.push(b':');
        prefix
    }
}

// Implement the DurableCollection trait for DurableVec<T>
impl<T> DurableCollection for DurableVec<T> 
where
    T: Serialize + for<'de> Deserialize<'de>
{
    fn from_prefix(db: Db, prefix: Vec<u8>) -> Self {
        DurableVec::from_prefix(db, prefix)
    }
}

/// Iterator over a DurableVec
pub struct VecIterator<'a, T> {
    inner: rocksdb::DBIterator<'a>,
    prefix: Vec<u8>,
    _phantom: PhantomData<T>,
}

impl<'a, T> Iterator for VecIterator<'a, T>
where
    T: for<'de> Deserialize<'de>
{
    type Item = Result<T>;
    
    fn next(&mut self) -> Option<Self::Item> {
        loop {
            match self.inner.next() {
                Some(Ok((key, value))) => {
                    // Check if we're still within our prefix
                    if !key.starts_with(&self.prefix) {
                        return None;
                    }
                    
                    // Check if this is a meta key (skip it)
                    // The key pattern is: prefix:element_index or prefix:__meta:type
                    // We want to skip any key that contains "__meta:"
                    if key.windows(7).any(|w| w == b"__meta:") {
                        continue; // Skip this key and try the next one
                    }
                    
                    // Deserialize the value
                    match bincode::deserialize(&value) {
                        Ok(item) => return Some(Ok(item)),
                        Err(e) => return Some(Err(e.into())),
                    }
                }
                Some(Err(e)) => return Some(Err(e.into())),
                None => return None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    
    fn setup_test_db() -> (TempDir, Db) {
        let temp_dir = TempDir::new().unwrap();
        let db = Db::open(temp_dir.path()).unwrap();
        (temp_dir, db)
    }
    
    #[test]
    fn test_push_and_get() {
        let (_temp, db) = setup_test_db();
        let mut vec = DurableVec::<String>::new(&db, "test_vec").unwrap();
        
        // Push some values
        vec.push("first".to_string()).unwrap();
        vec.push("second".to_string()).unwrap();
        vec.push("third".to_string()).unwrap();
        
        // Check length
        assert_eq!(vec.len().unwrap(), 3);
        
        // Get values
        assert_eq!(vec.get(0).unwrap(), Some("first".to_string()));
        assert_eq!(vec.get(1).unwrap(), Some("second".to_string()));
        assert_eq!(vec.get(2).unwrap(), Some("third".to_string()));
        assert_eq!(vec.get(3).unwrap(), None);
    }
    
    #[test]
    fn test_persistence() {
        let (temp_dir, db) = setup_test_db();
        
        // Create and populate vector
        {
            let mut vec = DurableVec::<i32>::new(&db, "persist_vec").unwrap();
            vec.push(42).unwrap();
            vec.push(100).unwrap();
            vec.push(-7).unwrap();
        }
        
        // Drop the database
        drop(db);
        
        // Reopen and verify data persists
        {
            let db = Db::open(temp_dir.path()).unwrap();
            let vec = DurableVec::<i32>::new(&db, "persist_vec").unwrap();
            
            assert_eq!(vec.len().unwrap(), 3);
            assert_eq!(vec.get(0).unwrap(), Some(42));
            assert_eq!(vec.get(1).unwrap(), Some(100));
            assert_eq!(vec.get(2).unwrap(), Some(-7));
        }
    }
    
    #[test]
    fn test_clear() {
        let (_temp, db) = setup_test_db();
        let mut vec = DurableVec::<u64>::new(&db, "clear_vec").unwrap();
        
        // Add some elements
        vec.extend(vec![1, 2, 3, 4, 5]).unwrap();
        assert_eq!(vec.len().unwrap(), 5);
        
        // Clear
        vec.clear().unwrap();
        assert_eq!(vec.len().unwrap(), 0);
        assert!(vec.is_empty().unwrap());
        
        // Should be able to push again
        vec.push(42).unwrap();
        assert_eq!(vec.len().unwrap(), 1);
        assert_eq!(vec.get(0).unwrap(), Some(42));
    }
    
    #[test]
    fn test_pop() {
        let (_temp, db) = setup_test_db();
        let mut vec = DurableVec::<String>::new(&db, "pop_vec").unwrap();
        
        // Empty pop
        assert_eq!(vec.pop().unwrap(), None);
        
        // Push and pop
        vec.push("a".to_string()).unwrap();
        vec.push("b".to_string()).unwrap();
        vec.push("c".to_string()).unwrap();
        
        assert_eq!(vec.pop().unwrap(), Some("c".to_string()));
        assert_eq!(vec.len().unwrap(), 2);
        assert_eq!(vec.pop().unwrap(), Some("b".to_string()));
        assert_eq!(vec.len().unwrap(), 1);
        assert_eq!(vec.pop().unwrap(), Some("a".to_string()));
        assert_eq!(vec.len().unwrap(), 0);
        assert_eq!(vec.pop().unwrap(), None);
    }
    
    #[test]
    fn test_iteration() {
        let (_temp, db) = setup_test_db();
        let mut vec = DurableVec::<i32>::new(&db, "iter_vec").unwrap();
        
        // Add elements
        let values = vec![10, 20, 30, 40, 50];
        vec.extend(values.clone()).unwrap();
        
        // Iterate and collect
        let collected = vec.to_vec().unwrap();
        
        assert_eq!(collected, values);
    }
    
    #[test]
    fn test_extend() {
        let (_temp, db) = setup_test_db();
        let mut vec = DurableVec::<String>::new(&db, "extend_vec").unwrap();
        
        // Extend with iterator
        vec.extend(vec!["a", "b", "c"].into_iter().map(String::from)).unwrap();
        assert_eq!(vec.len().unwrap(), 3);
        
        // Extend again
        vec.extend(vec!["d", "e"].into_iter().map(String::from)).unwrap();
        assert_eq!(vec.len().unwrap(), 5);
        
        // Verify all elements
        let all = vec.to_vec().unwrap();
        assert_eq!(all, vec!["a", "b", "c", "d", "e"]);
    }
    
    #[test]
    fn test_large_dataset() {
        let (_temp, db) = setup_test_db();
        let mut vec = DurableVec::<u64>::new(&db, "large_vec").unwrap();
        
        // Push many elements
        let count = 1000;
        for i in 0..count {
            vec.push(i).unwrap();
        }
        
        assert_eq!(vec.len().unwrap(), count as usize);
        
        // Verify some random accesses
        assert_eq!(vec.get(0).unwrap(), Some(0));
        assert_eq!(vec.get(500).unwrap(), Some(500));
        assert_eq!(vec.get(999).unwrap(), Some(999));
        assert_eq!(vec.get(1000).unwrap(), None);
        
        // Verify iteration count
        let all_values = vec.to_vec().unwrap();
        assert_eq!(all_values.len(), count as usize);
    }
    
    #[test] 
    fn test_complex_types() {
        use serde::{Serialize, Deserialize};
        
        #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
        struct User {
            id: u64,
            name: String,
            email: String,
            active: bool,
        }
        
        let (_temp, db) = setup_test_db();
        let mut vec = DurableVec::<User>::new(&db, "users").unwrap();
        
        let user1 = User {
            id: 1,
            name: "Alice".to_string(),
            email: "alice@example.com".to_string(),
            active: true,
        };
        
        let user2 = User {
            id: 2,
            name: "Bob".to_string(),
            email: "bob@example.com".to_string(),
            active: false,
        };
        
        vec.push(user1.clone()).unwrap();
        vec.push(user2.clone()).unwrap();
        
        assert_eq!(vec.get(0).unwrap(), Some(user1));
        assert_eq!(vec.get(1).unwrap(), Some(user2));
    }
    
    #[test]
    fn test_empty_vec_operations() {
        let (_temp, db) = setup_test_db();
        let vec = DurableVec::<i32>::new(&db, "empty_vec").unwrap();
        
        // Test operations on empty vec
        assert_eq!(vec.len().unwrap(), 0);
        assert!(vec.is_empty().unwrap());
        assert_eq!(vec.get(0).unwrap(), None);
        assert_eq!(vec.get(100).unwrap(), None);
        assert_eq!(vec.to_vec().unwrap(), Vec::<i32>::new());
    }
    
    #[test]
    fn test_multiple_vecs_same_db() {
        let (_temp, db) = setup_test_db();
        
        // Create multiple vectors with different names
        let mut vec1 = DurableVec::<String>::new(&db, "vec1").unwrap();
        let mut vec2 = DurableVec::<String>::new(&db, "vec2").unwrap();
        
        // Push different data to each
        vec1.push("vec1_data".to_string()).unwrap();
        vec2.push("vec2_data".to_string()).unwrap();
        
        // Verify they don't interfere
        assert_eq!(vec1.get(0).unwrap(), Some("vec1_data".to_string()));
        assert_eq!(vec2.get(0).unwrap(), Some("vec2_data".to_string()));
        assert_eq!(vec1.len().unwrap(), 1);
        assert_eq!(vec2.len().unwrap(), 1);
    }
    
    #[test]
    fn test_batch_atomicity() {
        let (_temp, db) = setup_test_db();
        let mut vec = DurableVec::<i32>::new(&db, "batch_vec").unwrap();
        
        // Add initial data
        vec.push(1).unwrap();
        vec.push(2).unwrap();
        vec.push(3).unwrap();
        
        // Verify initial state
        assert_eq!(vec.len().unwrap(), 3);
        
        // Clear should be atomic - either all elements deleted or none
        vec.clear().unwrap();
        assert_eq!(vec.len().unwrap(), 0);
        
        // Extend should be atomic - either all elements added or none
        vec.extend(vec![10, 20, 30, 40, 50]).unwrap();
        assert_eq!(vec.len().unwrap(), 5);
        let all = vec.to_vec().unwrap();
        assert_eq!(all, vec![10, 20, 30, 40, 50]);
    }
    
    #[test]
    fn test_unicode_strings() {
        let (_temp, db) = setup_test_db();
        let mut vec = DurableVec::<String>::new(&db, "unicode_vec").unwrap();
        
        let test_strings = vec![
            "Hello, 世界!".to_string(),
            "🦀 Rust 🚀".to_string(),
            "Ñoño".to_string(),
            "🏴‍☠️ Pirates".to_string(),
        ];
        
        vec.extend(test_strings.clone()).unwrap();
        
        let retrieved = vec.to_vec().unwrap();
        assert_eq!(retrieved, test_strings);
    }
    
    #[test]
    fn test_streaming_iterator() {
        let (_temp, db) = setup_test_db();
        let mut vec = DurableVec::<i32>::new(&db, "stream_vec").unwrap();
        
        // Add test data
        let values = vec![1, 2, 3, 4, 5];
        vec.extend(values.clone()).unwrap();
        
        // Test streaming iteration
        let mut collected = Vec::new();
        for item in vec.iter().unwrap() {
            collected.push(item.unwrap());
        }
        
        assert_eq!(collected, values);
        
        // Test that iterator properly handles prefix boundaries
        let mut vec2 = DurableVec::<i32>::new(&db, "stream_vec2").unwrap();
        vec2.extend(vec![10, 20, 30]).unwrap();
        
        // Each iterator should only see its own data
        let collected1: Vec<_> = vec.iter().unwrap().collect::<Result<Vec<_>>>().unwrap();
        let collected2: Vec<_> = vec2.iter().unwrap().collect::<Result<Vec<_>>>().unwrap();
        
        assert_eq!(collected1, values);
        assert_eq!(collected2, vec![10, 20, 30]);
    }
}

#[cfg(all(test, not(miri)))] // Skip proptest under miri
mod proptests {
    use super::*;
    use proptest::prelude::*;
    use tempfile::TempDir;
    
    fn setup_test_db() -> (TempDir, Db) {
        let temp_dir = TempDir::new().unwrap();
        let db = Db::open(temp_dir.path()).unwrap();
        (temp_dir, db)
    }
    
    proptest! {
        #[test]
        fn prop_push_get_consistency(values: Vec<i32>) {
            let (_temp, db) = setup_test_db();
            let mut vec = DurableVec::<i32>::new(&db, "prop_vec").unwrap();
            
            // Push all values
            for value in &values {
                vec.push(*value).unwrap();
            }
            
            // Verify length
            prop_assert_eq!(vec.len().unwrap(), values.len());
            
            // Verify all values can be retrieved correctly
            for (i, expected) in values.iter().enumerate() {
                prop_assert_eq!(vec.get(i).unwrap(), Some(*expected));
            }
        }
        
        #[test]
        fn prop_extend_iter_roundtrip(values: Vec<String>) {
            let (_temp, db) = setup_test_db();
            let mut vec = DurableVec::<String>::new(&db, "extend_vec").unwrap();
            
            // Extend with all values
            vec.extend(values.clone()).unwrap();
            
            // Get back via iteration
            let retrieved = vec.to_vec().unwrap();
            
            prop_assert_eq!(retrieved, values);
        }
        
        #[test]
        fn prop_pop_removes_last(mut values: Vec<u64>) {
            let (_temp, db) = setup_test_db();
            let mut vec = DurableVec::<u64>::new(&db, "pop_vec").unwrap();
            
            // Add all values
            vec.extend(values.clone()).unwrap();
            
            // Pop values and verify
            while let Some(expected) = values.pop() {
                let popped = vec.pop().unwrap();
                prop_assert_eq!(popped, Some(expected));
                prop_assert_eq!(vec.len().unwrap(), values.len());
            }
            
            // Vector should be empty
            prop_assert!(vec.is_empty().unwrap());
            prop_assert_eq!(vec.pop().unwrap(), None);
        }
        
        #[test]
        fn prop_clear_makes_empty(values: Vec<i32>) {
            let (_temp, db) = setup_test_db();
            let mut vec = DurableVec::<i32>::new(&db, "clear_vec").unwrap();
            
            // Add values
            vec.extend(values).unwrap();
            
            // Clear
            vec.clear().unwrap();
            
            // Should be empty
            prop_assert_eq!(vec.len().unwrap(), 0);
            prop_assert!(vec.is_empty().unwrap());
            prop_assert_eq!(vec.get(0).unwrap(), None);
        }
    }
} 