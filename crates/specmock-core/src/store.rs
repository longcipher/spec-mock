//! Retained invented resources for one scenario instance.
//!
//! A store answers the request a mock server receives most often and can least
//! afford to get wrong: *create this, then give it back to me*. Generated
//! payloads alone cannot do that, because every call is independent.
//!
//! Two properties are load-bearing and deliberate:
//!
//! - **Ordering is stable.** [`Store::list`] returns resources in creation order so a sequence of
//!   requests always produces the same sequence of responses.
//! - **Nothing is evicted.** Exceeding the capacity is an error the caller reports, never a silent
//!   drop. Evicting oldest-first would surface to a client as an unexplained `404`, which is
//!   exactly the failure mode this crate exists to avoid.
//!
//! Concurrency is handled with `scc` rather than a mutex, per `AGENTS.md`. The
//! data and the ordering index live in two maps so that lookups stay `O(1)`;
//! [`Store::list`] tolerates a key that is indexed but not yet present, which is
//! the one interleaving the split allows.

use scc::HashMap;
use serde_json::Value;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Namespace and identifier of a stored resource.
///
/// The namespace is a collection path template with its parameters already
/// bound, so `/users/5/orders` and `/orders` never share a slot for the
/// identifier `123`. Namespacing by path keeps distinct resources distinct
/// without inferring a type from the schema, which is unreliable when a spec
/// uses `oneOf` or omits a type name.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct StoreKey {
    /// Collection path template with its parameters bound.
    pub namespace: String,
    /// Identifier within the namespace.
    pub id: String,
}

impl StoreKey {
    /// Build a key from a namespace and identifier.
    #[must_use]
    pub fn new(namespace: impl Into<String>, id: impl Into<String>) -> Self {
        Self { namespace: namespace.into(), id: id.into() }
    }
}

impl std::fmt::Display for StoreKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}/{}", self.namespace, self.id)
    }
}

/// The operation a scenario declares against the store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreAction {
    /// Store a new resource, failing if the key is taken.
    Create,
    /// Read an existing resource.
    Read,
    /// Replace an existing resource.
    Update,
    /// Remove an existing resource.
    Delete,
    /// List a namespace in creation order.
    List,
}

/// Why a store operation could not be completed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StoreError {
    /// No resource exists at the key.
    #[error("no resource exists at {0}")]
    NotFound(StoreKey),
    /// A resource already exists at the key.
    #[error("a resource already exists at {0}")]
    AlreadyExists(StoreKey),
    /// The store reached its configured capacity.
    #[error("store capacity of {limit} entries exceeded")]
    CapacityExceeded {
        /// The configured limit.
        limit: usize,
    },
}

/// Retained resources for a single scenario instance.
///
/// Lock-free: reads and writes go through `scc` so concurrent requests do not
/// serialise on a mutex. Read-modify-write on one key is *not* atomic, which is
/// accepted and documented rather than engineered around -- two concurrent
/// `PUT`s to the same resource may interleave.
#[derive(Debug)]
pub struct Store {
    entries: HashMap<StoreKey, Value>,
    /// Insertion order of identifiers per namespace.
    order: HashMap<String, Vec<String>>,
    limit: usize,
    /// Live entry count, atomically maintained alongside `entries`.
    count: AtomicUsize,
}

impl Store {
    /// Build a store bounded to `limit` entries across all namespaces.
    #[must_use]
    pub fn new(limit: usize) -> Self {
        Self { entries: HashMap::new(), order: HashMap::new(), limit, count: AtomicUsize::new(0) }
    }

    /// Store a new resource, failing if the key is taken or the store is full.
    ///
    /// Collision detection is delegated to `insert_sync`, so two concurrent
    /// creates for one key cannot both succeed. The capacity check is
    /// necessarily advisory: it can be overtaken by a racing create.
    pub fn create(&self, key: &StoreKey, value: Value) -> Result<(), StoreError> {
        // Atomically reserve a slot before touching the maps, so concurrent
        // creates cannot both pass the capacity check and overshoot the cap.
        loop {
            let current = self.count.load(Ordering::Acquire);
            if current >= self.limit {
                return Err(StoreError::CapacityExceeded { limit: self.limit });
            }
            if self.count.compare_exchange(current, current + 1, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                break;
            }
        }

        // `insert_sync` fails if the key is already present, which makes the
        // collision check and the write a single atomic step.
        if self.entries.insert_sync(key.clone(), value).is_err() {
            // Release the slot we reserved; the collision is not a stored entry.
            self.count.fetch_sub(1, Ordering::AcqRel);
            return Err(StoreError::AlreadyExists(key.clone()));
        }
        // `update_sync` only touches an existing key, so the namespace's
        // ordering entry has to be created before it can be appended to.
        let namespace = key.namespace.clone();
        let id = key.id.clone();
        if !self.order.contains_sync(&namespace) {
            self.order.insert_sync(namespace.clone(), vec![id]).ok();
        } else {
            self.order.update_sync(&namespace, |_, ids| ids.push(id));
        }
        Ok(())
    }

    /// Read a resource, or `None` when absent.
    ///
    /// The value is cloned rather than lent: `scc` hands back an entry guard,
    /// and returning one would leak the concurrency primitive into the API.
    /// Stored payloads are small mock documents, so the copy is not worth an API
    /// that constrains every caller.
    #[must_use]
    pub fn read(&self, key: &StoreKey) -> Option<Value> {
        self.entries.get_sync(key).map(|entry| entry.get().clone())
    }

    /// Replace an existing resource, failing when absent.
    pub fn update(&self, key: &StoreKey, value: Value) -> Result<(), StoreError> {
        self.entries
            .update_sync(key, |_, slot| *slot = value)
            .ok_or_else(|| StoreError::NotFound(key.clone()))?;
        Ok(())
    }

    /// Remove an existing resource, failing when absent.
    pub fn delete(&self, key: &StoreKey) -> Result<(), StoreError> {
        self.entries
            .remove_sync(key)
            .map(|_| ())
            .ok_or_else(|| StoreError::NotFound(key.clone()))?;
        self.count.fetch_sub(1, Ordering::AcqRel);
        let namespace = key.namespace.clone();
        let id = key.id.clone();
        self.order.update_sync(&namespace, |_, ids| ids.retain(|existing| existing != &id));
        Ok(())
    }

    /// List a namespace in creation order.
    ///
    /// An identifier indexed but not yet written is skipped, which is the one
    /// interleaving the split between `entries` and `order` permits.
    #[must_use]
    pub fn list(&self, namespace: &str) -> Vec<Value> {
        let Some(entry) = self.order.get_sync(namespace) else {
            return Vec::new();
        };
        entry
            .get()
            .iter()
            .filter_map(|id| {
                self.entries
                    .get_sync(&StoreKey::new(namespace, id.clone()))
                    .map(|e| e.get().clone())
            })
            .collect()
    }

    /// Number of stored resources across all namespaces.
    #[must_use]
    pub fn len(&self) -> usize {
        self.count.load(Ordering::Acquire)
    }

    /// Whether the store holds no resources.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Configured capacity.
    #[must_use]
    pub fn limit(&self) -> usize {
        self.limit
    }

    /// Drop every resource and return how many were removed.
    ///
    /// Clearing the data and the ordering index together keeps a reset from
    /// leaving a namespace listing identifiers that no longer resolve.
    pub fn clear(&self) -> usize {
        let removed = self.count.load(Ordering::Acquire);
        self.entries.clear_sync();
        self.order.clear_sync();
        self.count.store(0, Ordering::Release);
        removed
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn key(id: &str) -> StoreKey {
        StoreKey::new("/users", id)
    }

    #[test]
    fn create_then_read_returns_the_same_value() {
        let store = Store::new(10);
        store.create(&key("1"), json!({"id": "1"})).expect("create should succeed");
        assert_eq!(store.read(&key("1")), Some(json!({"id": "1"})));
    }

    #[test]
    fn create_twice_at_one_key_collides() {
        let store = Store::new(10);
        store.create(&key("1"), json!({"id": "1"})).expect("first create should succeed");
        assert_eq!(
            store.create(&key("1"), json!({"id": "other"})),
            Err(StoreError::AlreadyExists(key("1"))),
            "a second create must not overwrite"
        );
        assert_eq!(store.read(&key("1")), Some(json!({"id": "1"})), "the original must survive");
    }

    #[test]
    fn update_requires_an_existing_resource() {
        let store = Store::new(10);
        assert_eq!(
            store.update(&key("missing"), json!({})),
            Err(StoreError::NotFound(key("missing"))),
            "update is not an upsert"
        );
    }

    #[test]
    fn delete_requires_an_existing_resource_and_then_removes_it() {
        let store = Store::new(10);
        assert_eq!(store.delete(&key("missing")), Err(StoreError::NotFound(key("missing"))));

        store.create(&key("1"), json!({"id": "1"})).expect("create should succeed");
        store.delete(&key("1")).expect("delete should succeed");
        assert_eq!(store.read(&key("1")), None, "a deleted resource stays deleted");
    }

    #[test]
    fn list_returns_creation_order() {
        let store = Store::new(10);
        for id in ["c", "a", "b"] {
            store.create(&key(id), json!({"id": id})).expect("create should succeed");
        }
        let ids: Vec<String> = store
            .list("/users")
            .into_iter()
            .map(|value| value["id"].as_str().unwrap_or_default().to_owned())
            .collect();
        assert_eq!(ids, ["c", "a", "b"], "list must follow creation, not sort order");
    }

    #[test]
    fn an_empty_namespace_lists_nothing() {
        let store = Store::new(10);
        assert!(
            store.list("/users").is_empty(),
            "an empty store must list nothing rather than inventing items"
        );
    }

    #[test]
    fn namespaces_do_not_leak_into_each_other() {
        let store = Store::new(10);
        store.create(&StoreKey::new("/users", "1"), json!({"kind": "user"})).expect("create");
        store
            .create(&StoreKey::new("/users/5/orders", "1"), json!({"kind": "order"}))
            .expect("create");

        assert_eq!(store.read(&StoreKey::new("/users", "1")), Some(json!({"kind": "user"})));
        assert_eq!(
            store.read(&StoreKey::new("/users/5/orders", "1")),
            Some(json!({"kind": "order"})),
            "the same identifier in two namespaces is two resources"
        );
    }

    #[test]
    fn create_beyond_capacity_reports_rather_than_evicts() {
        let store = Store::new(2);
        store.create(&key("1"), json!(1)).expect("first");
        store.create(&key("2"), json!(2)).expect("second");

        assert_eq!(
            store.create(&key("3"), json!(3)),
            Err(StoreError::CapacityExceeded { limit: 2 })
        );
        assert_eq!(
            store.read(&key("1")),
            Some(json!(1)),
            "hitting the cap must not drop anything that already exists"
        );
        assert_eq!(store.len(), 2);
    }

    #[test]
    fn delete_frees_capacity() {
        let store = Store::new(1);
        store.create(&key("1"), json!(1)).expect("first");
        store.delete(&key("1")).expect("delete");
        store.create(&key("2"), json!(2)).expect("capacity freed by delete");
    }

    #[test]
    fn clear_empties_data_and_ordering_together() {
        let store = Store::new(10);
        store.create(&key("1"), json!(1)).expect("create");
        store.create(&key("2"), json!(2)).expect("create");

        assert_eq!(store.clear(), 2);
        assert!(store.is_empty());
        assert!(store.list("/users").is_empty(), "reset must not leave a stale ordering index");

        store.create(&key("3"), json!(3)).expect("create after clear");
        assert_eq!(store.list("/users"), vec![json!(3)], "listing restarts in order");
    }

    #[test]
    fn deleting_then_creating_the_same_id_reorders_the_list() {
        let store = Store::new(10);
        store.create(&key("1"), json!(1)).expect("create");
        store.create(&key("2"), json!(2)).expect("create");
        store.delete(&key("1")).expect("delete");
        store.create(&key("1"), json!(1)).expect("recreate");

        assert_eq!(
            store.list("/users"),
            vec![json!(2), json!(1)],
            "a recreated resource must land at the end, not its original slot"
        );
    }

    #[test]
    fn concurrent_creates_all_land() {
        use std::sync::Arc;

        let store = Arc::new(Store::new(256));
        let handles: Vec<_> = (0..64)
            .map(|index| {
                let store = Arc::clone(&store);
                std::thread::spawn(move || {
                    store.create(&key(&index.to_string()), json!(index)).expect("create");
                })
            })
            .collect();
        for handle in handles {
            handle.join().expect("thread should not panic");
        }

        assert_eq!(store.len(), 64);
        assert_eq!(store.list("/users").len(), 64);
    }
}
