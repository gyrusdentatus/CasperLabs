mod byte_size;
mod ext;
pub(self) mod meter;
#[cfg(test)]
mod tests;

use std::{
    collections::{HashMap, HashSet, VecDeque},
    convert::From,
    iter,
};

use linked_hash_map::LinkedHashMap;

use engine_shared::{
    additive_map::AdditiveMap,
    newtypes::CorrelationId,
    stored_value::StoredValue,
    transform::{self, Transform},
    TypeMismatch,
};
use engine_storage::global_state::StateReader;
use types::{bytesrepr, CLType, CLValueError, Key};

use crate::engine_state::{execution_effect::ExecutionEffect, op::Op};

pub use self::ext::TrackingCopyExt;
use self::meter::{heap_meter::HeapSize, Meter};

#[derive(Debug)]
pub enum TrackingCopyQueryResult {
    Success(StoredValue),
    ValueNotFound(String),
    CircularReference(String),
}

/// Struct containing state relating to a given query.
struct Query {
    /// The key from where the search starts.
    base_key: Key,
    /// A collection of normalized keys which have been visited during the search.
    visited_keys: HashSet<Key>,
    /// The key currently being processed.
    current_key: Key,
    /// Path components which have not yet been followed, held in the same order in which they were
    /// provided to the `query()` call.
    unvisited_names: VecDeque<String>,
    /// Path components which have been followed, held in the same order in which they were
    /// provided to the `query()` call.
    visited_names: Vec<String>,
}

impl Query {
    fn new(base_key: Key, path: &[String]) -> Self {
        Query {
            base_key,
            current_key: base_key.normalize(),
            unvisited_names: path.iter().cloned().collect(),
            visited_names: Vec::new(),
            visited_keys: HashSet::new(),
        }
    }

    /// Panics if `unvisited_names` is empty.
    fn next_name(&mut self) -> &String {
        let next_name = self.unvisited_names.pop_front().unwrap();
        self.visited_names.push(next_name);
        self.visited_names.last().unwrap()
    }

    fn into_not_found_result(self, msg_prefix: &str) -> TrackingCopyQueryResult {
        let msg = format!("{} at path: {}", msg_prefix, self.current_path());
        TrackingCopyQueryResult::ValueNotFound(msg)
    }

    fn into_circular_ref_result(self) -> TrackingCopyQueryResult {
        let msg = format!(
            "{:?} has formed a circular reference at path: {}",
            self.current_key,
            self.current_path()
        );
        TrackingCopyQueryResult::CircularReference(msg)
    }

    fn current_path(&self) -> String {
        let mut path = format!("{:?}", self.base_key);
        for name in &self.visited_names {
            path.push_str("/");
            path.push_str(name);
        }
        path
    }
}

/// Keeps track of already accessed keys.
/// We deliberately separate cached Reads from cached mutations
/// because we want to invalidate Reads' cache so it doesn't grow too fast.
pub struct TrackingCopyCache<M> {
    max_cache_size: usize,
    current_cache_size: usize,
    reads_cached: LinkedHashMap<Key, StoredValue>,
    muts_cached: HashMap<Key, StoredValue>,
    meter: M,
}

impl<M: Meter<Key, StoredValue>> TrackingCopyCache<M> {
    /// Creates instance of `TrackingCopyCache` with specified `max_cache_size`,
    /// above which least-recently-used elements of the cache are invalidated.
    /// Measurements of elements' "size" is done with the usage of `Meter`
    /// instance.
    pub fn new(max_cache_size: usize, meter: M) -> TrackingCopyCache<M> {
        TrackingCopyCache {
            max_cache_size,
            current_cache_size: 0,
            reads_cached: LinkedHashMap::new(),
            muts_cached: HashMap::new(),
            meter,
        }
    }

    /// Inserts `key` and `value` pair to Read cache.
    pub fn insert_read(&mut self, key: Key, value: StoredValue) {
        let element_size = Meter::measure(&self.meter, &key, &value);
        self.reads_cached.insert(key, value);
        self.current_cache_size += element_size;
        while self.current_cache_size > self.max_cache_size {
            match self.reads_cached.pop_front() {
                Some((k, v)) => {
                    let element_size = Meter::measure(&self.meter, &k, &v);
                    self.current_cache_size -= element_size;
                }
                None => break,
            }
        }
    }

    /// Inserts `key` and `value` pair to Write/Add cache.
    pub fn insert_write(&mut self, key: Key, value: StoredValue) {
        self.muts_cached.insert(key, value);
    }

    /// Gets value from `key` in the cache.
    pub fn get(&mut self, key: &Key) -> Option<&StoredValue> {
        if let Some(value) = self.muts_cached.get(&key) {
            return Some(value);
        };

        self.reads_cached.get_refresh(key).map(|v| &*v)
    }
}

pub struct TrackingCopy<R> {
    reader: R,
    cache: TrackingCopyCache<HeapSize>,
    ops: AdditiveMap<Key, Op>,
    fns: AdditiveMap<Key, Transform>,
}

#[derive(Debug)]
pub enum AddResult {
    Success,
    KeyNotFound(Key),
    TypeMismatch(TypeMismatch),
    Serialization(bytesrepr::Error),
}

impl From<CLValueError> for AddResult {
    fn from(error: CLValueError) -> Self {
        match error {
            CLValueError::Serialization(error) => AddResult::Serialization(error),
            CLValueError::Type(type_mismatch) => {
                let expected = format!("{:?}", type_mismatch.expected);
                let found = format!("{:?}", type_mismatch.found);
                AddResult::TypeMismatch(TypeMismatch::new(expected, found))
            }
        }
    }
}

impl<R: StateReader<Key, StoredValue>> TrackingCopy<R> {
    pub fn new(reader: R) -> TrackingCopy<R> {
        TrackingCopy {
            reader,
            cache: TrackingCopyCache::new(1024 * 16, HeapSize),
            /* TODO: Should `max_cache_size`
             * be fraction of wasm memory
             * limit? */
            ops: AdditiveMap::new(),
            fns: AdditiveMap::new(),
        }
    }

    pub fn reader(&self) -> &R {
        &self.reader
    }

    /// Creates a new TrackingCopy, using this one (including its mutations) as
    /// the base state to read against. The intended use case for this
    /// function is to "snapshot" the current `TrackingCopy` and produce a
    /// new `TrackingCopy` where further changes can be made. This
    /// allows isolating a specific set of changes (those in the new
    /// `TrackingCopy`) from existing changes. Note that mutations to state
    /// caused by new changes (i.e. writes and adds) only impact the new
    /// `TrackingCopy`, not this one. Note that currently there is no `join` /
    /// `merge` function to bring changes from a fork back to the main
    /// `TrackingCopy`. this means the current usage requires repeated
    /// forking, however we recognize this is sub-optimal and will revisit
    /// in the future.
    pub fn fork(&self) -> TrackingCopy<&TrackingCopy<R>> {
        TrackingCopy::new(self)
    }

    pub fn get(
        &mut self,
        correlation_id: CorrelationId,
        key: &Key,
    ) -> Result<Option<StoredValue>, R::Error> {
        if let Some(value) = self.cache.get(key) {
            return Ok(Some(value.to_owned()));
        }
        if let Some(value) = self.reader.read(correlation_id, key)? {
            self.cache.insert_read(*key, value.to_owned());
            Ok(Some(value))
        } else {
            Ok(None)
        }
    }

    pub fn read(
        &mut self,
        correlation_id: CorrelationId,
        key: &Key,
    ) -> Result<Option<StoredValue>, R::Error> {
        let normalized_key = key.normalize();
        if let Some(value) = self.get(correlation_id, &normalized_key)? {
            self.ops.insert_add(normalized_key, Op::Read);
            self.fns.insert_add(normalized_key, Transform::Identity);
            Ok(Some(value))
        } else {
            Ok(None)
        }
    }

    pub fn write(&mut self, key: Key, value: StoredValue) {
        let normalized_key = key.normalize();
        self.cache.insert_write(normalized_key, value.clone());
        self.ops.insert_add(normalized_key, Op::Write);
        self.fns.insert_add(normalized_key, Transform::Write(value));
    }

    /// Ok(None) represents missing key to which we want to "add" some value.
    /// Ok(Some(unit)) represents successful operation.
    /// Err(error) is reserved for unexpected errors when accessing global
    /// state.
    pub fn add(
        &mut self,
        correlation_id: CorrelationId,
        key: Key,
        value: StoredValue,
    ) -> Result<AddResult, R::Error> {
        let normalized_key = key.normalize();
        let current_value = match self.get(correlation_id, &normalized_key)? {
            None => return Ok(AddResult::KeyNotFound(normalized_key)),
            Some(current_value) => current_value,
        };

        let type_name = value.type_name();
        let mismatch = || {
            Ok(AddResult::TypeMismatch(TypeMismatch::new(
                "I32, U64, U128, U256, U512 or (String, Key) tuple".to_string(),
                type_name,
            )))
        };

        let transform = match value {
            StoredValue::CLValue(cl_value) => match *cl_value.cl_type() {
                CLType::I32 => match cl_value.into_t() {
                    Ok(value) => Transform::AddInt32(value),
                    Err(error) => return Ok(AddResult::from(error)),
                },
                CLType::U64 => match cl_value.into_t() {
                    Ok(value) => Transform::AddUInt64(value),
                    Err(error) => return Ok(AddResult::from(error)),
                },
                CLType::U128 => match cl_value.into_t() {
                    Ok(value) => Transform::AddUInt128(value),
                    Err(error) => return Ok(AddResult::from(error)),
                },
                CLType::U256 => match cl_value.into_t() {
                    Ok(value) => Transform::AddUInt256(value),
                    Err(error) => return Ok(AddResult::from(error)),
                },
                CLType::U512 => match cl_value.into_t() {
                    Ok(value) => Transform::AddUInt512(value),
                    Err(error) => return Ok(AddResult::from(error)),
                },
                _ => {
                    if *cl_value.cl_type() == types::named_key_type() {
                        match cl_value.into_t() {
                            Ok(name_and_key) => {
                                let map = iter::once(name_and_key).collect();
                                Transform::AddKeys(map)
                            }
                            Err(error) => return Ok(AddResult::from(error)),
                        }
                    } else {
                        return mismatch();
                    }
                }
            },
            _ => return mismatch(),
        };

        match transform.clone().apply(current_value) {
            Ok(new_value) => {
                self.cache.insert_write(normalized_key, new_value);
                self.ops.insert_add(normalized_key, Op::Add);
                self.fns.insert_add(normalized_key, transform);
                Ok(AddResult::Success)
            }
            Err(transform::Error::TypeMismatch(type_mismatch)) => {
                Ok(AddResult::TypeMismatch(type_mismatch))
            }
            Err(transform::Error::Serialization(error)) => Ok(AddResult::Serialization(error)),
        }
    }

    pub fn effect(&self) -> ExecutionEffect {
        ExecutionEffect::new(self.ops.clone(), self.fns.clone())
    }

    /// Calling `query()` avoids calling into `self.cache`, so this will not return any values
    /// written or mutated in this `TrackingCopy` via previous calls to `write()` or `add()`, since
    /// these updates are only held in `self.cache`.
    ///
    /// The intent is that `query()` is only used to satisfy `QueryRequest`s made to the server.
    /// Other EE internal use cases should call `read()` or `get()` in order to retrieve cached
    /// values.
    pub fn query(
        &self,
        correlation_id: CorrelationId,
        base_key: Key,
        path: &[String],
    ) -> Result<TrackingCopyQueryResult, R::Error> {
        let mut query = Query::new(base_key, path);

        loop {
            if !query.visited_keys.insert(query.current_key) {
                return Ok(query.into_circular_ref_result());
            }
            let stored_value = match self.reader.read(correlation_id, &query.current_key)? {
                None => {
                    return Ok(query.into_not_found_result("Failed to find base key"));
                }
                Some(stored_value) => stored_value,
            };

            if query.unvisited_names.is_empty() {
                return Ok(TrackingCopyQueryResult::Success(stored_value));
            }

            match stored_value {
                StoredValue::Account(account) => {
                    let name = query.next_name();
                    if let Some(key) = account.named_keys().get(name) {
                        query.current_key = key.normalize();
                    } else {
                        let msg_prefix = format!("Name {} not found in Account", name);
                        return Ok(query.into_not_found_result(&msg_prefix));
                    }
                }
                StoredValue::CLValue(cl_value) if cl_value.cl_type() == &CLType::Key => {
                    if let Ok(key) = cl_value.into_t::<Key>() {
                        query.current_key = key.normalize();
                    } else {
                        return Ok(query.into_not_found_result("Failed to parse CLValue as Key"));
                    }
                }
                StoredValue::CLValue(cl_value) => {
                    let msg_prefix = format!(
                        "Query cannot continue as {:?} is not an account, contract nor key to \
                        such.  Value found",
                        cl_value
                    );
                    return Ok(query.into_not_found_result(&msg_prefix));
                }
                StoredValue::Contract(contract) => {
                    let name = query.next_name();
                    if let Some(key) = contract.named_keys().get(name) {
                        query.current_key = key.normalize();
                    } else {
                        let msg_prefix = format!("Name {} not found in Contract", name);
                        return Ok(query.into_not_found_result(&msg_prefix));
                    }
                }
                StoredValue::ContractPackage(_) => {
                    return Ok(query.into_not_found_result(&"ContractPackage value found."));
                }
                StoredValue::ContractWasm(_) => {
                    return Ok(query.into_not_found_result(&"ContractWasm value found."));
                }
            }
        }
    }
}

/// The purpose of this implementation is to allow a "snapshot" mechanism for
/// TrackingCopy. The state of a TrackingCopy (including the effects of
/// any transforms it has accumulated) can be read using an immutable
/// reference to that TrackingCopy via this trait implementation. See
/// `TrackingCopy::fork` for more information.
impl<R: StateReader<Key, StoredValue>> StateReader<Key, StoredValue> for &TrackingCopy<R> {
    type Error = R::Error;

    fn read(
        &self,
        correlation_id: CorrelationId,
        key: &Key,
    ) -> Result<Option<StoredValue>, Self::Error> {
        if let Some(value) = self.cache.muts_cached.get(key) {
            return Ok(Some(value.to_owned()));
        }
        if let Some(value) = self.reader.read(correlation_id, key)? {
            Ok(Some(value))
        } else {
            Ok(None)
        }
    }
}
