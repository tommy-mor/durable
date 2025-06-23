use crate::{Db, Result, DurableError};
use rocksdb::{IteratorMode, WriteBatch, Direction};
use serde::{Serialize, Deserialize};
use std::marker::PhantomData;

/// A persistent map backed by RocksDB
pub struct DurableMap<K, V> {
    db: Db,
    prefix: Vec<u8>,
    _phantom: PhantomData<(K, V)>,
}

impl<K, V> DurableMap<K, V> 
where 
    K: Serialize + for<'de> Deserialize<'de>,
    V: Serialize + for<'de> Deserialize<'de>,
{
    /// Create a new DurableMap with the given name
    pub fn new(db: &Db, name: &str) -> Result<Self> {
        let prefix = format!("map:{}", name).into_bytes();
        
        Ok(DurableMap {
            db: db.clone(),
            prefix,
            _phantom: PhantomData,
        })
    }
    
    /// Insert a key-value pair into the map
    pub fn insert(&mut self, key: K, value: V) -> Result<Option<V>> {
        let key_bytes = bincode::serialize(&key)?;
        let value_bytes = bincode::serialize(&value)?;
        
        // Get the old value if it exists
        let old_value = self.get(&key)?;
        
        let mut batch = WriteBatch::default();
        
        // Write the new value
        let db_key = self.entry_key(&key_bytes);
        batch.put(&db_key, &value_bytes);
        
        // Update length if this is a new key
        if old_value.is_none() {
            let new_len = self.len()? + 1;
            let len_key = self.meta_key("len");
            batch.put(&len_key, &(new_len as u64).to_le_bytes());
        }
        
        // Commit atomically
        self.db.rocks().write(batch)?;
        self.db.rocks().flush_wal(true)?;
        
        Ok(old_value)
    }
    
    /// Put a key-value pair into the map without returning the old value
    /// 
    /// This is more efficient than `insert` when you don't need the old value,
    /// as it only checks for key existence without deserializing the value.
    pub fn put(&mut self, key: K, value: V) -> Result<()> {
        let key_bytes = bincode::serialize(&key)?;
        let value_bytes = bincode::serialize(&value)?;
        let db_key = self.entry_key(&key_bytes);
        
        let mut batch = WriteBatch::default();
        
        // Check if this is a new key (without deserializing the value)
        let is_new = self.db.rocks().get_pinned(&db_key)?.is_none();
        
        // Write the new value
        batch.put(&db_key, &value_bytes);
        
        // Update length if this is a new key
        if is_new {
            let new_len = self.len()? + 1;
            let len_key = self.meta_key("len");
            batch.put(&len_key, &(new_len as u64).to_le_bytes());
        }
        
        // Commit atomically
        self.db.rocks().write(batch)?;
        self.db.rocks().flush_wal(true)?;
        
        Ok(())
    }
    
    /// Get a value by key
    pub fn get(&self, key: &K) -> Result<Option<V>> {
        let key_bytes = bincode::serialize(key)?;
        let db_key = self.entry_key(&key_bytes);
        
        match self.db.rocks().get(&db_key)? {
            Some(bytes) => {
                let value = bincode::deserialize(&bytes)?;
                Ok(Some(value))
            }
            None => Ok(None),
        }
    }
    
    /// Check if a key exists in the map
    pub fn contains_key(&self, key: &K) -> Result<bool> {
        let key_bytes = bincode::serialize(key)?;
        let db_key = self.entry_key(&key_bytes);
        
        Ok(self.db.rocks().get(&db_key)?.is_some())
    }
    
    /// Remove a key-value pair from the map
    pub fn remove(&mut self, key: &K) -> Result<Option<V>> {
        let key_bytes = bincode::serialize(key)?;
        let db_key = self.entry_key(&key_bytes);
        
        // Get the old value
        let old_value = match self.db.rocks().get(&db_key)? {
            Some(bytes) => {
                let value = bincode::deserialize(&bytes)?;
                Some(value)
            }
            None => None,
        };
        
        // Delete the key if it existed and update length
        if old_value.is_some() {
            let mut batch = WriteBatch::default();
            
            // Delete the entry
            batch.delete(&db_key);
            
            // Update length
            let new_len = self.len()? - 1;
            let len_key = self.meta_key("len");
            batch.put(&len_key, &(new_len as u64).to_le_bytes());
            
            // Commit atomically
            self.db.rocks().write(batch)?;
            self.db.rocks().flush_wal(true)?;
        }
        
        Ok(old_value)
    }
    
    /// Get the number of entries in the map
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
    
    /// Check if the map is empty
    pub fn is_empty(&self) -> Result<bool> {
        Ok(self.len()? == 0)
    }
    
    /// Clear all entries from the map
    pub fn clear(&mut self) -> Result<()> {
        let prefix = self.entry_prefix();
        let mut batch = WriteBatch::default();
        
        // Collect all keys to delete
        let iter = self.db.rocks().iterator(IteratorMode::From(&prefix, Direction::Forward));
        for item in iter {
            let (key, _) = item?;
            if !key.starts_with(&prefix) {
                break;
            }
            batch.delete(&key);
        }
        
        // Reset length to 0
        let len_key = self.meta_key("len");
        batch.delete(&len_key);
        
        // Commit atomically
        self.db.rocks().write(batch)?;
        self.db.rocks().flush_wal(true)?;
        
        Ok(())
    }
    
    /// Iterate over all key-value pairs using a streaming iterator
    pub fn iter(&self) -> MapIterator<'_, K, V> {
        let prefix = self.entry_prefix();
        let iter = self.db.rocks().iterator(IteratorMode::From(&prefix, Direction::Forward));
        
        MapIterator {
            inner: iter,
            prefix,
            _phantom: PhantomData,
        }
    }
    
    /// Load all key-value pairs into a Vec
    /// 
    /// Note: This loads the entire collection into memory. For large collections,
    /// prefer using `iter()` which streams elements.
    pub fn to_vec(&self) -> Result<Vec<(K, V)>> {
        let mut result = Vec::new();
        for item in self.iter() {
            result.push(item?);
        }
        Ok(result)
    }
    
    /// Iterate over all keys using a streaming iterator
    pub fn keys(&self) -> KeyIterator<'_, K, V> {
        let prefix = self.entry_prefix();
        let iter = self.db.rocks().iterator(IteratorMode::From(&prefix, Direction::Forward));
        
        KeyIterator {
            inner: iter,
            prefix,
            _phantom: PhantomData,
        }
    }
    
    /// Load all keys into a Vec
    /// 
    /// Note: This loads all keys into memory. For large collections,
    /// prefer using `keys()` which streams elements.
    pub fn keys_vec(&self) -> Result<Vec<K>> {
        let mut result = Vec::new();
        for item in self.keys() {
            result.push(item?);
        }
        Ok(result)
    }
    
    /// Iterate over all values using a streaming iterator
    pub fn values(&self) -> ValueIterator<'_, K, V> {
        let prefix = self.entry_prefix();
        let iter = self.db.rocks().iterator(IteratorMode::From(&prefix, Direction::Forward));
        
        ValueIterator {
            inner: iter,
            prefix,
            _phantom: PhantomData,
        }
    }
    
    /// Load all values into a Vec
    /// 
    /// Note: This loads all values into memory. For large collections,
    /// prefer using `values()` which streams elements.
    pub fn values_vec(&self) -> Result<Vec<V>> {
        let mut result = Vec::new();
        for item in self.values() {
            result.push(item?);
        }
        Ok(result)
    }
    
    /// Insert multiple key-value pairs in a single batch
    pub fn extend<I>(&mut self, iter: I) -> Result<()>
    where
        I: IntoIterator<Item = (K, V)>
    {
        let mut batch = WriteBatch::default();
        let current_len = self.len()?;
        let mut new_entries = 0;
        
        for (key, value) in iter {
            let key_bytes = bincode::serialize(&key)?;
            let value_bytes = bincode::serialize(&value)?;
            let db_key = self.entry_key(&key_bytes);
            
            // Check if this is a new key
            if !self.contains_key(&key)? {
                new_entries += 1;
            }
            
            batch.put(&db_key, &value_bytes);
        }
        
        // Update length if we added new entries
        if new_entries > 0 {
            let new_len = current_len + new_entries;
            let len_key = self.meta_key("len");
            batch.put(&len_key, &(new_len as u64).to_le_bytes());
        }
        
        // Commit atomically
        self.db.rocks().write(batch)?;
        self.db.rocks().flush_wal(true)?;
        
        Ok(())
    }
    
    // Helper methods
    
    fn entry_key(&self, key_bytes: &[u8]) -> Vec<u8> {
        let mut db_key = self.prefix.clone();
        db_key.extend_from_slice(b":entry:");
        db_key.extend_from_slice(key_bytes);
        db_key
    }
    
    fn entry_prefix(&self) -> Vec<u8> {
        let mut prefix = self.prefix.clone();
        prefix.extend_from_slice(b":entry:");
        prefix
    }
    
    fn meta_key(&self, meta_type: &str) -> Vec<u8> {
        let mut key = self.prefix.clone();
        key.extend_from_slice(b":__meta:");
        key.extend_from_slice(meta_type.as_bytes());
        key
    }
}

/// Iterator over key-value pairs in a DurableMap
pub struct MapIterator<'a, K, V> {
    inner: rocksdb::DBIterator<'a>,
    prefix: Vec<u8>,
    _phantom: PhantomData<(K, V)>,
}

impl<'a, K, V> Iterator for MapIterator<'a, K, V>
where
    K: for<'de> Deserialize<'de>,
    V: for<'de> Deserialize<'de>,
{
    type Item = Result<(K, V)>;
    
    fn next(&mut self) -> Option<Self::Item> {
        match self.inner.next() {
            Some(Ok((db_key, value_bytes))) => {
                // Check if we're still within our prefix
                if !db_key.starts_with(&self.prefix) {
                    return None;
                }
                
                // Extract the key part (skip prefix)
                let key_start = self.prefix.len();
                let key_bytes = &db_key[key_start..];
                
                // Deserialize key and value
                match (bincode::deserialize(key_bytes), bincode::deserialize(&value_bytes)) {
                    (Ok(key), Ok(value)) => Some(Ok((key, value))),
                    (Err(e), _) | (_, Err(e)) => Some(Err(e.into())),
                }
            }
            Some(Err(e)) => Some(Err(e.into())),
            None => None,
        }
    }
}

/// Iterator over keys in a DurableMap
pub struct KeyIterator<'a, K, V> {
    inner: rocksdb::DBIterator<'a>,
    prefix: Vec<u8>,
    _phantom: PhantomData<(K, V)>,
}

impl<'a, K, V> Iterator for KeyIterator<'a, K, V>
where
    K: for<'de> Deserialize<'de>,
{
    type Item = Result<K>;
    
    fn next(&mut self) -> Option<Self::Item> {
        match self.inner.next() {
            Some(Ok((db_key, _))) => {
                // Check if we're still within our prefix
                if !db_key.starts_with(&self.prefix) {
                    return None;
                }
                
                // Extract the key part (skip prefix)
                let key_start = self.prefix.len();
                let key_bytes = &db_key[key_start..];
                
                // Deserialize key
                match bincode::deserialize(key_bytes) {
                    Ok(key) => Some(Ok(key)),
                    Err(e) => Some(Err(e.into())),
                }
            }
            Some(Err(e)) => Some(Err(e.into())),
            None => None,
        }
    }
}

/// Iterator over values in a DurableMap
pub struct ValueIterator<'a, K, V> {
    inner: rocksdb::DBIterator<'a>,
    prefix: Vec<u8>,
    _phantom: PhantomData<(K, V)>,
}

impl<'a, K, V> Iterator for ValueIterator<'a, K, V>
where
    V: for<'de> Deserialize<'de>,
{
    type Item = Result<V>;
    
    fn next(&mut self) -> Option<Self::Item> {
        match self.inner.next() {
            Some(Ok((db_key, value_bytes))) => {
                // Check if we're still within our prefix
                if !db_key.starts_with(&self.prefix) {
                    return None;
                }
                
                // Deserialize value
                match bincode::deserialize(&value_bytes) {
                    Ok(value) => Some(Ok(value)),
                    Err(e) => Some(Err(e.into())),
                }
            }
            Some(Err(e)) => Some(Err(e.into())),
            None => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use std::collections::HashMap;
    
    fn setup_test_db() -> (TempDir, Db) {
        let temp_dir = TempDir::new().unwrap();
        let db = Db::open(temp_dir.path()).unwrap();
        (temp_dir, db)
    }
    
    #[test]
    fn test_insert_and_get() {
        let (_temp, db) = setup_test_db();
        let mut map = DurableMap::<String, i32>::new(&db, "test_map").unwrap();
        
        // Insert some values
        assert_eq!(map.insert("one".to_string(), 1).unwrap(), None);
        assert_eq!(map.insert("two".to_string(), 2).unwrap(), None);
        assert_eq!(map.insert("three".to_string(), 3).unwrap(), None);
        
        // Get values
        assert_eq!(map.get(&"one".to_string()).unwrap(), Some(1));
        assert_eq!(map.get(&"two".to_string()).unwrap(), Some(2));
        assert_eq!(map.get(&"three".to_string()).unwrap(), Some(3));
        assert_eq!(map.get(&"four".to_string()).unwrap(), None);
        
        // Update existing value
        assert_eq!(map.insert("two".to_string(), 22).unwrap(), Some(2));
        assert_eq!(map.get(&"two".to_string()).unwrap(), Some(22));
    }
    
    #[test]
    fn test_remove() {
        let (_temp, db) = setup_test_db();
        let mut map = DurableMap::<String, String>::new(&db, "remove_map").unwrap();
        
        // Insert and remove
        map.insert("key".to_string(), "value".to_string()).unwrap();
        assert_eq!(map.remove(&"key".to_string()).unwrap(), Some("value".to_string()));
        assert_eq!(map.remove(&"key".to_string()).unwrap(), None);
        assert_eq!(map.get(&"key".to_string()).unwrap(), None);
    }
    
    #[test]
    fn test_contains_key() {
        let (_temp, db) = setup_test_db();
        let mut map = DurableMap::<i32, String>::new(&db, "contains_map").unwrap();
        
        map.insert(42, "answer".to_string()).unwrap();
        
        assert!(map.contains_key(&42).unwrap());
        assert!(!map.contains_key(&43).unwrap());
    }
    
    #[test]
    fn test_len_and_clear() {
        let (_temp, db) = setup_test_db();
        let mut map = DurableMap::<u64, u64>::new(&db, "len_map").unwrap();
        
        // Empty map
        assert_eq!(map.len().unwrap(), 0);
        assert!(map.is_empty().unwrap());
        
        // Add items
        for i in 0..10 {
            map.insert(i, i * 2).unwrap();
        }
        assert_eq!(map.len().unwrap(), 10);
        assert!(!map.is_empty().unwrap());
        
        // Clear
        map.clear().unwrap();
        assert_eq!(map.len().unwrap(), 0);
        assert!(map.is_empty().unwrap());
    }
    
    #[test]
    fn test_persistence() {
        let (temp_dir, db) = setup_test_db();
        
        // Create and populate map
        {
            let mut map = DurableMap::<String, Vec<u8>>::new(&db, "persist_map").unwrap();
            map.insert("binary".to_string(), vec![1, 2, 3, 4, 5]).unwrap();
            map.insert("data".to_string(), vec![10, 20, 30]).unwrap();
        }
        
        // Drop the database
        drop(db);
        
        // Reopen and verify data persists
        {
            let db = Db::open(temp_dir.path()).unwrap();
            let map = DurableMap::<String, Vec<u8>>::new(&db, "persist_map").unwrap();
            
            assert_eq!(map.get(&"binary".to_string()).unwrap(), Some(vec![1, 2, 3, 4, 5]));
            assert_eq!(map.get(&"data".to_string()).unwrap(), Some(vec![10, 20, 30]));
            assert_eq!(map.len().unwrap(), 2);
        }
    }
    
    #[test]
    fn test_iteration() {
        let (_temp, db) = setup_test_db();
        let mut map = DurableMap::<String, i32>::new(&db, "iter_map").unwrap();
        
        // Insert data
        let data = vec![
            ("apple".to_string(), 1),
            ("banana".to_string(), 2),
            ("cherry".to_string(), 3),
        ];
        
        for (k, v) in &data {
            map.insert(k.clone(), *v).unwrap();
        }
        
        // Test iter()
        let mut items = map.to_vec().unwrap();
        items.sort_by_key(|(k, _)| k.clone());
        assert_eq!(items, data);
        
        // Test keys()
        let mut keys = map.keys_vec().unwrap();
        keys.sort();
        assert_eq!(keys, vec!["apple", "banana", "cherry"]);
        
        // Test values()
        let mut values = map.values_vec().unwrap();
        values.sort();
        assert_eq!(values, vec![1, 2, 3]);
    }
    
    #[test]
    fn test_extend() {
        let (_temp, db) = setup_test_db();
        let mut map = DurableMap::<i32, String>::new(&db, "extend_map").unwrap();
        
        // Extend from iterator
        let data: HashMap<i32, String> = vec![
            (1, "one".to_string()),
            (2, "two".to_string()),
            (3, "three".to_string()),
        ].into_iter().collect();
        
        map.extend(data.clone()).unwrap();
        
        // Verify all items were inserted
        for (k, v) in data {
            assert_eq!(map.get(&k).unwrap(), Some(v));
        }
        assert_eq!(map.len().unwrap(), 3);
    }
    
    #[test]
    fn test_complex_keys() {
        use serde::{Serialize, Deserialize};
        
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
        struct ComplexKey {
            id: u64,
            name: String,
        }
        
        let (_temp, db) = setup_test_db();
        let mut map = DurableMap::<ComplexKey, String>::new(&db, "complex_map").unwrap();
        
        let key1 = ComplexKey { id: 1, name: "first".to_string() };
        let key2 = ComplexKey { id: 2, name: "second".to_string() };
        
        map.insert(key1.clone(), "value1".to_string()).unwrap();
        map.insert(key2.clone(), "value2".to_string()).unwrap();
        
        assert_eq!(map.get(&key1).unwrap(), Some("value1".to_string()));
        assert_eq!(map.get(&key2).unwrap(), Some("value2".to_string()));
    }
    
    #[test]
    fn test_multiple_maps_same_db() {
        let (_temp, db) = setup_test_db();
        
        let mut map1 = DurableMap::<String, i32>::new(&db, "map1").unwrap();
        let mut map2 = DurableMap::<String, i32>::new(&db, "map2").unwrap();
        
        // Insert different data
        map1.insert("shared_key".to_string(), 100).unwrap();
        map2.insert("shared_key".to_string(), 200).unwrap();
        
        // Verify isolation
        assert_eq!(map1.get(&"shared_key".to_string()).unwrap(), Some(100));
        assert_eq!(map2.get(&"shared_key".to_string()).unwrap(), Some(200));
    }
    
    #[test]
    fn test_streaming_iterators() {
        let (_temp, db) = setup_test_db();
        let mut map = DurableMap::<String, i32>::new(&db, "stream_map").unwrap();
        
        // Insert test data
        let data = vec![
            ("alice".to_string(), 100),
            ("bob".to_string(), 200),
            ("charlie".to_string(), 300),
        ];
        
        for (k, v) in &data {
            map.insert(k.clone(), *v).unwrap();
        }
        
        // Test streaming iteration
        let mut collected = Vec::new();
        for item in map.iter() {
            let (k, v) = item.unwrap();
            collected.push((k, v));
        }
        collected.sort_by_key(|(k, _)| k.clone());
        assert_eq!(collected, data);
        
        // Test keys iterator
        let mut keys = Vec::new();
        for key in map.keys() {
            keys.push(key.unwrap());
        }
        keys.sort();
        assert_eq!(keys, vec!["alice", "bob", "charlie"]);
        
        // Test values iterator
        let mut values = Vec::new();
        for value in map.values() {
            values.push(value.unwrap());
        }
        values.sort();
        assert_eq!(values, vec![100, 200, 300]);
        
        // Test that iterators properly handle prefix boundaries
        let mut map2 = DurableMap::<String, i32>::new(&db, "stream_map2").unwrap();
        map2.insert("dave".to_string(), 400).unwrap();
        
        // Each iterator should only see its own data
        let collected1: Vec<_> = map.iter().map(Result::unwrap).collect();
        let collected2: Vec<_> = map2.iter().map(Result::unwrap).collect();
        
        assert_eq!(collected1.len(), 3);
        assert_eq!(collected2.len(), 1);
        assert_eq!(collected2[0], ("dave".to_string(), 400));
    }
    
    #[test]
    fn test_metadata_length_tracking() {
        let (_temp, db) = setup_test_db();
        let mut map = DurableMap::<String, String>::new(&db, "length_map").unwrap();
        
        // Empty map
        assert_eq!(map.len().unwrap(), 0);
        assert!(map.is_empty().unwrap());
        
        // Insert operations should update length
        map.insert("key1".to_string(), "value1".to_string()).unwrap();
        assert_eq!(map.len().unwrap(), 1);
        
        map.insert("key2".to_string(), "value2".to_string()).unwrap();
        assert_eq!(map.len().unwrap(), 2);
        
        // Updating existing key should not change length
        map.insert("key1".to_string(), "new_value1".to_string()).unwrap();
        assert_eq!(map.len().unwrap(), 2);
        
        // Remove operations should update length
        map.remove(&"key1".to_string()).unwrap();
        assert_eq!(map.len().unwrap(), 1);
        
        // Removing non-existent key should not change length
        map.remove(&"non_existent".to_string()).unwrap();
        assert_eq!(map.len().unwrap(), 1);
        
        // Extend should update length correctly
        let data = vec![
            ("key3".to_string(), "value3".to_string()),
            ("key4".to_string(), "value4".to_string()),
            ("key5".to_string(), "value5".to_string()),
        ];
        map.extend(data).unwrap();
        assert_eq!(map.len().unwrap(), 4); // key2 + 3 new keys
        
        // Extend with existing keys should only count new ones
        let mixed_data = vec![
            ("key2".to_string(), "updated_value2".to_string()), // existing
            ("key6".to_string(), "value6".to_string()),         // new
        ];
        map.extend(mixed_data).unwrap();
        assert_eq!(map.len().unwrap(), 5); // only key6 was new
        
        // Clear should reset length to 0
        map.clear().unwrap();
        assert_eq!(map.len().unwrap(), 0);
        assert!(map.is_empty().unwrap());
    }
    
    #[test]
    fn test_put_method() {
        let (_temp, db) = setup_test_db();
        let mut map = DurableMap::<String, i32>::new(&db, "put_map").unwrap();
        
        // Put new entries
        map.put("a".to_string(), 1).unwrap();
        map.put("b".to_string(), 2).unwrap();
        map.put("c".to_string(), 3).unwrap();
        
        // Verify entries exist and length is correct
        assert_eq!(map.get(&"a".to_string()).unwrap(), Some(1));
        assert_eq!(map.get(&"b".to_string()).unwrap(), Some(2));
        assert_eq!(map.get(&"c".to_string()).unwrap(), Some(3));
        assert_eq!(map.len().unwrap(), 3);
        
        // Update existing entry with put
        map.put("b".to_string(), 20).unwrap();
        assert_eq!(map.get(&"b".to_string()).unwrap(), Some(20));
        assert_eq!(map.len().unwrap(), 3); // Length should not change
        
        // Compare put vs insert performance characteristics
        // put() doesn't return old value but is more efficient
        map.put("d".to_string(), 4).unwrap();
        assert_eq!(map.len().unwrap(), 4);
        
        // insert() returns old value
        let old = map.insert("d".to_string(), 40).unwrap();
        assert_eq!(old, Some(4));
        assert_eq!(map.len().unwrap(), 4);
    }
}

#[cfg(all(test, not(miri)))]
mod proptests {
    use super::*;
    use proptest::prelude::*;
    use tempfile::TempDir;
    use std::collections::HashMap;
    
    fn setup_test_db() -> (TempDir, Db) {
        let temp_dir = TempDir::new().unwrap();
        let db = Db::open(temp_dir.path()).unwrap();
        (temp_dir, db)
    }
    
    proptest! {
        #[test]
        fn prop_insert_get_consistency(data: HashMap<String, i32>) {
            let (_temp, db) = setup_test_db();
            let mut map = DurableMap::<String, i32>::new(&db, "prop_map").unwrap();
            
            // Insert all pairs
            for (k, v) in &data {
                map.insert(k.clone(), *v).unwrap();
            }
            
            // Verify all can be retrieved
            for (k, v) in &data {
                prop_assert_eq!(map.get(k).unwrap(), Some(*v));
            }
            
            // Verify length
            prop_assert_eq!(map.len().unwrap(), data.len());
        }
        
        #[test]
        fn prop_remove_consistency(data: HashMap<u32, String>) {
            let (_temp, db) = setup_test_db();
            let mut map = DurableMap::<u32, String>::new(&db, "remove_map").unwrap();
            
            // Insert all
            map.extend(data.clone()).unwrap();
            
            // Remove all and verify
            for (k, v) in data {
                prop_assert_eq!(map.remove(&k).unwrap(), Some(v));
                prop_assert_eq!(map.remove(&k).unwrap(), None);
                prop_assert!(!map.contains_key(&k).unwrap());
            }
            
            prop_assert!(map.is_empty().unwrap());
        }
        
        #[test]
        fn prop_clear_makes_empty(data: HashMap<i64, i64>) {
            let (_temp, db) = setup_test_db();
            let mut map = DurableMap::<i64, i64>::new(&db, "clear_map").unwrap();
            
            map.extend(data).unwrap();
            map.clear().unwrap();
            
            prop_assert_eq!(map.len().unwrap(), 0);
            prop_assert!(map.is_empty().unwrap());
            prop_assert_eq!(map.to_vec().unwrap(), vec![]);
        }
    }
} 