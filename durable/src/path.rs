//! Typed paths: composable, data-only addresses into a durable schema.
//!
//! A [`Path<S>`] is just a byte prefix plus a phantom schema type. Navigation
//! methods are gated by the concrete schema, so only legal steps compile, and
//! terminal operations return reified [`Write`]s (for mutations) or read directly
//! from a [`Db`].

use std::marker::PhantomData;

use serde::{de::DeserializeOwned, Serialize};

use crate::{
    codec,
    schema::{decode_sum, encode_sum, Deque, Leaf, List, Map, Schema, Sum, Summable},
    decode_value, encode_value, read_i64, read_u64, Db, Error, Op, Result, Write,
};

/// A typed address into a durable schema.
///
/// Cheap to clone; carries only the lowered key prefix. Construct the root of a
/// schema with [`Path::root`] (typically via the `#[derive(Durable)]`-generated
/// `S::root()`), then navigate with schema-specific methods.
pub struct Path<S> {
    prefix: Vec<u8>,
    _schema: PhantomData<fn() -> S>,
}

impl<S> Clone for Path<S> {
    fn clone(&self) -> Self {
        Self {
            prefix: self.prefix.clone(),
            _schema: PhantomData,
        }
    }
}

impl<S> std::fmt::Debug for Path<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Path")
            .field("schema", &std::any::type_name::<S>())
            .field("prefix", &self.prefix)
            .finish()
    }
}

impl<S: Schema> Path<S> {
    /// The empty-prefixed root of a schema.
    ///
    /// One root schema per database. Navigate from here.
    pub fn root() -> Self {
        Self::from_prefix(Vec::new())
    }

    /// A root namespaced under `name`, so multiple schemas can share one database.
    pub fn namespaced(name: &str) -> Self {
        let mut prefix = Vec::new();
        codec::put_segment(&mut prefix, name.as_bytes());
        Self::from_prefix(prefix)
    }
}

impl<S> Path<S> {
    pub(crate) fn from_prefix(prefix: Vec<u8>) -> Self {
        Self {
            prefix,
            _schema: PhantomData,
        }
    }

    /// The lowered RocksDB key prefix this path addresses.
    pub fn prefix(&self) -> &[u8] {
        &self.prefix
    }

    fn child<S2>(&self, seg: &[u8]) -> Path<S2> {
        Path::from_prefix(codec::child_key(&self.prefix, seg))
    }

    /// Navigate to a `#[derive(Durable)]` struct field. Called by generated code.
    #[doc(hidden)]
    pub fn child_field<S2>(&self, field_id: u32) -> Path<S2> {
        let mut seg = Vec::new();
        codec::put_uvarint(&mut seg, field_id as u64);
        self.child(&seg)
    }
}

// ---------------------------------------------------------------------------
// Leaf<T>
// ---------------------------------------------------------------------------

impl<T: Serialize + DeserializeOwned> Path<Leaf<T>> {
    /// Read the value at this leaf, if present.
    pub fn get(&self, db: &Db) -> Result<Option<T>> {
        match db.raw().get(&self.prefix)? {
            Some(bytes) => Ok(Some(decode_value(&bytes)?)),
            None => Ok(None),
        }
    }

    /// A reified blind write that sets this leaf to `value`.
    pub fn set(&self, value: &T) -> Write {
        let bytes = encode_value(value).expect("durable: leaf value serialization failed");
        Write::new(Op::Put {
            key: self.prefix.clone(),
            value: bytes,
        })
    }

    /// A reified blind write that removes this leaf.
    pub fn delete(&self) -> Write {
        Write::new(Op::Delete {
            key: self.prefix.clone(),
        })
    }
}

// ---------------------------------------------------------------------------
// Sum<N>
// ---------------------------------------------------------------------------

impl<N: Summable> Path<Sum<N>> {
    /// Read the accumulated value (defaults to zero when absent).
    pub fn get(&self, db: &Db) -> Result<N> {
        match db.raw().get(&self.prefix)? {
            Some(bytes) => decode_sum::<N>(&bytes)
                .ok_or_else(|| Error::Corruption("malformed Sum accumulator".into())),
            None => Ok(N::zero()),
        }
    }

    /// A reified blind merge that adds `delta` to the accumulator.
    ///
    /// This never reads the current value: it is an O(1) write whose effect is
    /// resolved lazily by RocksDB's merge operator.
    pub fn add(&self, delta: N) -> Write {
        Write::new(Op::Merge {
            key: self.prefix.clone(),
            value: encode_sum(delta),
        })
    }

    /// A reified blind write that sets the accumulator to an exact value.
    pub fn set(&self, value: N) -> Write {
        Write::new(Op::Put {
            key: self.prefix.clone(),
            value: encode_sum(value),
        })
    }

    /// A reified blind write that removes the accumulator.
    pub fn delete(&self) -> Write {
        Write::new(Op::Delete {
            key: self.prefix.clone(),
        })
    }
}

// ---------------------------------------------------------------------------
// Map<K, V>
// ---------------------------------------------------------------------------

impl<K: Serialize, V: Schema> Path<Map<K, V>> {
    /// Navigate to the sub-schema stored under `key`.
    pub fn key(&self, key: &K) -> Path<V> {
        let encoded = encode_value(key).expect("durable: map key serialization failed");
        self.child(&encoded)
    }

    /// A reified write that deletes the entire map (all entries and metadata).
    pub fn clear(&self) -> Write {
        Write::new(Op::DeletePrefix {
            prefix: self.prefix.clone(),
        })
    }
}

impl<K: Serialize + DeserializeOwned, V: Schema> Path<Map<K, V>> {
    /// All keys present in the map, in stored (encoded-byte) order.
    pub fn keys(&self, db: &Db) -> Result<Vec<K>> {
        let scan = codec::child_scan_prefix(&self.prefix);
        let iter = db
            .raw()
            .iterator(rocksdb::IteratorMode::From(&scan, rocksdb::Direction::Forward));
        let mut keys = Vec::new();
        let mut last: Option<Vec<u8>> = None;
        for item in iter {
            let (db_key, _) = item?;
            if !db_key.starts_with(&scan) {
                break;
            }
            let rest = &db_key[scan.len()..];
            let (key_seg, _) = codec::read_segment(rest)
                .ok_or_else(|| Error::Corruption("malformed map entry key".into()))?;
            if last.as_deref() == Some(key_seg) {
                continue; // same logical key, deeper sub-key
            }
            last = Some(key_seg.to_vec());
            keys.push(decode_value(key_seg)?);
        }
        Ok(keys)
    }

    /// The number of distinct keys in the map.
    pub fn len(&self, db: &Db) -> Result<usize> {
        Ok(self.keys(db)?.len())
    }

    /// Whether the map has no entries.
    pub fn is_empty(&self, db: &Db) -> Result<bool> {
        let scan = codec::child_scan_prefix(&self.prefix);
        let mut iter = db
            .raw()
            .iterator(rocksdb::IteratorMode::From(&scan, rocksdb::Direction::Forward));
        match iter.next() {
            Some(item) => {
                let (db_key, _) = item?;
                Ok(!db_key.starts_with(&scan))
            }
            None => Ok(true),
        }
    }

    /// Whether `key` is present.
    pub fn contains(&self, db: &Db, key: &K) -> Result<bool> {
        let child = self.key(key);
        // A present entry has at least one key at-or-under the child prefix.
        let mut iter = db.raw().iterator(rocksdb::IteratorMode::From(
            child.prefix(),
            rocksdb::Direction::Forward,
        ));
        match iter.next() {
            Some(item) => {
                let (db_key, _) = item?;
                Ok(db_key.starts_with(child.prefix()))
            }
            None => Ok(false),
        }
    }

    /// All keys paired with sub-paths into their values (composable navigation).
    pub fn entries(&self, db: &Db) -> Result<Vec<(K, Path<V>)>> {
        let keys = self.keys(db)?;
        Ok(keys
            .into_iter()
            .map(|k| {
                let path = self.key(&k);
                (k, path)
            })
            .collect())
    }
}

// Leaf-valued maps gain direct value iteration and bulk transforms.
impl<K: Serialize + DeserializeOwned, T: Serialize + DeserializeOwned> Path<Map<K, Leaf<T>>> {
    /// Read the value stored under `key`.
    pub fn get(&self, db: &Db, key: &K) -> Result<Option<T>> {
        self.key(key).get(db)
    }

    /// All `(key, value)` pairs in stored order.
    pub fn iter(&self, db: &Db) -> Result<Vec<(K, T)>> {
        let scan = codec::child_scan_prefix(&self.prefix);
        let iter = db
            .raw()
            .iterator(rocksdb::IteratorMode::From(&scan, rocksdb::Direction::Forward));
        let mut out = Vec::new();
        for item in iter {
            let (db_key, value) = item?;
            if !db_key.starts_with(&scan) {
                break;
            }
            let rest = &db_key[scan.len()..];
            let (key_seg, used) = codec::read_segment(rest)
                .ok_or_else(|| Error::Corruption("malformed map entry key".into()))?;
            // Leaf entries are exactly one physical key; reject deeper sub-keys.
            if used != rest.len() {
                return Err(Error::Corruption("unexpected nested key in leaf map".into()));
            }
            out.push((decode_value(key_seg)?, decode_value(&value)?));
        }
        Ok(out)
    }

    /// Build reified writes that rewrite each value through `f`.
    ///
    /// Returning `Some(new)` sets the value, `None` deletes the entry. This reads
    /// the map once (a prefix scan) and yields blind writes, so the whole
    /// transform commits atomically in one batch (e.g. "decay every edge weight").
    pub fn transform_values(
        &self,
        db: &Db,
        mut f: impl FnMut(&K, T) -> Option<T>,
    ) -> Result<Vec<Write>> {
        let mut writes = Vec::new();
        for (k, v) in self.iter(db)? {
            let entry = self.key(&k);
            match f(&k, v) {
                Some(new) => writes.push(entry.set(&new)),
                None => writes.push(entry.delete()),
            }
        }
        Ok(writes)
    }
}

// Sum-valued maps gain direct accumulator iteration and bulk transforms.
impl<K: Serialize + DeserializeOwned, N: Summable> Path<Map<K, Sum<N>>> {
    /// Read the accumulator stored under `key` (zero when absent).
    pub fn get(&self, db: &Db, key: &K) -> Result<N> {
        self.key(key).get(db)
    }

    /// All `(key, value)` accumulator pairs in stored order.
    pub fn iter(&self, db: &Db) -> Result<Vec<(K, N)>> {
        let scan = codec::child_scan_prefix(&self.prefix);
        let iter = db
            .raw()
            .iterator(rocksdb::IteratorMode::From(&scan, rocksdb::Direction::Forward));
        let mut out = Vec::new();
        for item in iter {
            let (db_key, value) = item?;
            if !db_key.starts_with(&scan) {
                break;
            }
            let rest = &db_key[scan.len()..];
            let (key_seg, used) = codec::read_segment(rest)
                .ok_or_else(|| Error::Corruption("malformed map entry key".into()))?;
            if used != rest.len() {
                return Err(Error::Corruption("unexpected nested key in sum map".into()));
            }
            let n = decode_sum::<N>(&value)
                .ok_or_else(|| Error::Corruption("malformed Sum accumulator".into()))?;
            out.push((decode_value(key_seg)?, n));
        }
        Ok(out)
    }

    /// Build reified writes that rewrite each accumulator through `f`.
    ///
    /// `Some(new)` sets the accumulator (blind put), `None` deletes it. Reads the
    /// map once, yields blind writes — ideal for "decay every edge weight".
    pub fn transform_values(
        &self,
        db: &Db,
        mut f: impl FnMut(&K, N) -> Option<N>,
    ) -> Result<Vec<Write>> {
        let mut writes = Vec::new();
        for (k, v) in self.iter(db)? {
            let entry = self.key(&k);
            match f(&k, v) {
                Some(new) => writes.push(entry.set(new)),
                None => writes.push(entry.delete()),
            }
        }
        Ok(writes)
    }
}

// ---------------------------------------------------------------------------
// List<V>
// ---------------------------------------------------------------------------

impl<V: Schema> Path<List<V>> {
    /// Navigate to the element at `index` (no bounds check until read).
    pub fn at(&self, index: u64) -> Path<V> {
        self.child(&codec::order_u64(index))
    }

    /// The number of elements.
    pub fn len(&self, db: &Db) -> Result<u64> {
        Ok(read_u64(db, &codec::meta_key(&self.prefix, b"len"))?.unwrap_or(0))
    }

    /// Whether the list is empty.
    pub fn is_empty(&self, db: &Db) -> Result<bool> {
        Ok(self.len(db)? == 0)
    }

    /// A reified write that deletes the whole list (elements and length).
    pub fn clear(&self) -> Write {
        Write::new(Op::DeletePrefix {
            prefix: self.prefix.clone(),
        })
    }
}

impl<T: Serialize + DeserializeOwned> Path<List<Leaf<T>>> {
    /// Read the element at `index`.
    pub fn get(&self, db: &Db, index: u64) -> Result<Option<T>> {
        if index >= self.len(db)? {
            return Ok(None);
        }
        self.at(index).get(db)
    }

    /// A reified list append, resolved against the collection length at commit.
    ///
    /// This is the form a reducer hands to [`crate::Tx::write`]. It does not
    /// touch the database.
    pub fn push_op(&self, value: &T) -> Write {
        let bytes = encode_value(value).expect("durable: list value serialization failed");
        Write::new(Op::ListPush {
            prefix: self.prefix.clone(),
            value: bytes,
        })
    }

    /// Append `value`, returning its index. Commits with `SyncWal`.
    pub fn push(&self, db: &Db, value: &T) -> Result<u64> {
        let mut batch = db.batch();
        let index = self.len(db)?;
        batch.push(self, value)?;
        batch.commit()?;
        Ok(index)
    }

    /// Remove and return the last element. Commits with `SyncWal`.
    pub fn pop(&self, db: &Db) -> Result<Option<T>> {
        let len = self.len(db)?;
        if len == 0 {
            return Ok(None);
        }
        let last = len - 1;
        let value = self.at(last).get(db)?;
        let mut batch = db.batch();
        batch.write(self.at(last).delete());
        batch.raw_put(codec::meta_key(&self.prefix, b"len"), last.to_le_bytes().to_vec());
        batch.commit()?;
        Ok(value)
    }

    /// All elements in index order.
    pub fn iter(&self, db: &Db) -> Result<Vec<T>> {
        let len = self.len(db)?;
        let mut out = Vec::with_capacity(len as usize);
        for i in 0..len {
            match self.at(i).get(db)? {
                Some(v) => out.push(v),
                None => return Err(Error::Corruption("list element missing below len".into())),
            }
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// Deque<V>
// ---------------------------------------------------------------------------

impl<V: Schema> Path<Deque<V>> {
    fn head(&self, db: &Db) -> Result<i64> {
        Ok(read_i64(db, &codec::meta_key(&self.prefix, b"head"))?.unwrap_or(0))
    }

    fn tail(&self, db: &Db) -> Result<i64> {
        Ok(read_i64(db, &codec::meta_key(&self.prefix, b"tail"))?.unwrap_or(0))
    }

    /// The number of elements.
    pub fn len(&self, db: &Db) -> Result<u64> {
        Ok((self.tail(db)? - self.head(db)?).max(0) as u64)
    }

    /// Whether the deque is empty.
    pub fn is_empty(&self, db: &Db) -> Result<bool> {
        Ok(self.len(db)? == 0)
    }

    /// A reified write that deletes the whole deque (elements and metadata).
    pub fn clear(&self) -> Write {
        Write::new(Op::DeletePrefix {
            prefix: self.prefix.clone(),
        })
    }
}

impl<T: Serialize + DeserializeOwned> Path<Deque<Leaf<T>>> {
    /// A reified deque-back append, resolved at commit.
    pub fn push_back_op(&self, value: &T) -> Write {
        let bytes = encode_value(value).expect("durable: deque value serialization failed");
        Write::new(Op::DequePushBack {
            prefix: self.prefix.clone(),
            value: bytes,
        })
    }

    /// A reified deque-front push, resolved at commit.
    pub fn push_front_op(&self, value: &T) -> Write {
        let bytes = encode_value(value).expect("durable: deque value serialization failed");
        Write::new(Op::DequePushFront {
            prefix: self.prefix.clone(),
            value: bytes,
        })
    }

    /// Push to the back. Commits with `SyncWal`.
    pub fn push_back(&self, db: &Db, value: &T) -> Result<()> {
        let mut batch = db.batch();
        batch.push_back(self, value)?;
        batch.commit()
    }

    /// Push to the front. Commits with `SyncWal`.
    pub fn push_front(&self, db: &Db, value: &T) -> Result<()> {
        let mut batch = db.batch();
        batch.push_front(self, value)?;
        batch.commit()
    }

    /// Remove and return the front element. Commits with `SyncWal`.
    pub fn pop_front(&self, db: &Db) -> Result<Option<T>> {
        let head = self.head(db)?;
        let tail = self.tail(db)?;
        if head >= tail {
            return Ok(None);
        }
        let value = self.child::<Leaf<T>>(&codec::order_i64(head)).get(db)?;
        let mut batch = db.batch();
        batch.write(self.child::<Leaf<T>>(&codec::order_i64(head)).delete());
        batch.raw_put(
            codec::meta_key(&self.prefix, b"head"),
            (head + 1).to_le_bytes().to_vec(),
        );
        batch.commit()?;
        Ok(value)
    }

    /// Remove and return the back element. Commits with `SyncWal`.
    pub fn pop_back(&self, db: &Db) -> Result<Option<T>> {
        let head = self.head(db)?;
        let tail = self.tail(db)?;
        if head >= tail {
            return Ok(None);
        }
        let last = tail - 1;
        let value = self.child::<Leaf<T>>(&codec::order_i64(last)).get(db)?;
        let mut batch = db.batch();
        batch.write(self.child::<Leaf<T>>(&codec::order_i64(last)).delete());
        batch.raw_put(
            codec::meta_key(&self.prefix, b"tail"),
            last.to_le_bytes().to_vec(),
        );
        batch.commit()?;
        Ok(value)
    }

    /// Read the front element without removing it.
    pub fn front(&self, db: &Db) -> Result<Option<T>> {
        let head = self.head(db)?;
        if head >= self.tail(db)? {
            return Ok(None);
        }
        self.child::<Leaf<T>>(&codec::order_i64(head)).get(db)
    }

    /// Read the back element without removing it.
    pub fn back(&self, db: &Db) -> Result<Option<T>> {
        let tail = self.tail(db)?;
        if self.head(db)? >= tail {
            return Ok(None);
        }
        self.child::<Leaf<T>>(&codec::order_i64(tail - 1)).get(db)
    }

    /// All elements from front to back.
    pub fn iter(&self, db: &Db) -> Result<Vec<T>> {
        let head = self.head(db)?;
        let tail = self.tail(db)?;
        let mut out = Vec::with_capacity((tail - head).max(0) as usize);
        for idx in head..tail {
            match self.child::<Leaf<T>>(&codec::order_i64(idx)).get(db)? {
                Some(v) => out.push(v),
                None => return Err(Error::Corruption("deque element missing in range".into())),
            }
        }
        Ok(out)
    }

    /// Drop elements from the back until the length is at most `max_len`,
    /// committing with the given durability. A no-op when already short enough.
    pub fn truncate_back(
        &self,
        db: &Db,
        max_len: u64,
        durability: crate::Durability,
    ) -> Result<()> {
        let head = self.head(db)?;
        let tail = self.tail(db)?;
        let len = (tail - head).max(0) as u64;
        if len <= max_len {
            return Ok(());
        }
        let new_tail = tail - (len - max_len) as i64;
        let mut batch = db.batch();
        for idx in new_tail..tail {
            batch.write(self.child::<Leaf<T>>(&codec::order_i64(idx)).delete());
        }
        batch.raw_put(
            codec::meta_key(&self.prefix, b"tail"),
            new_tail.to_le_bytes().to_vec(),
        );
        batch.commit_with(durability)
    }
}
