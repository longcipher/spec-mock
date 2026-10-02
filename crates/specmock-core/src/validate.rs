//! JSON schema validation helpers.

use std::{
    hash::{DefaultHasher, Hash, Hasher},
    sync::{
        Arc, LazyLock,
        atomic::{AtomicUsize, Ordering},
    },
};

use jsonschema::{Validator, validator_for};
use scc::HashMap;
use serde_json::Value;

use crate::error::{SpecMockCoreError, ValidationIssue};

const DEFAULT_VALIDATOR_CACHE_MAX_ENTRIES: usize = 256;

static VALIDATOR_CACHE: LazyLock<HashMap<u64, Arc<Validator>>> = LazyLock::new(HashMap::new);
static VALIDATOR_CACHE_MAX_ENTRIES: AtomicUsize =
    AtomicUsize::new(DEFAULT_VALIDATOR_CACHE_MAX_ENTRIES);

/// Hash a schema into a stable cache key.
///
/// The value tree is hashed in place rather than serialized first: validation
/// runs on every request, and materialising a full JSON string per call was the
/// dominant cost of the cache lookup.  Every container is length-prefixed so
/// structurally different schemas cannot hash to the same key by reordering
/// members.
fn hash_value(value: &Value) -> u64 {
    let mut hasher = DefaultHasher::new();
    hash_json_value(value, &mut hasher);
    hasher.finish()
}

fn hash_json_value<H: Hasher>(value: &Value, hasher: &mut H) {
    match value {
        Value::Null => hasher.write_u8(0),
        Value::Bool(flag) => {
            hasher.write_u8(1);
            hasher.write_u8(u8::from(*flag));
        }
        Value::Number(number) => {
            hasher.write_u8(2);
            number.to_string().hash(hasher);
        }
        Value::String(text) => {
            hasher.write_u8(3);
            text.hash(hasher);
        }
        Value::Array(items) => {
            hasher.write_u8(4);
            hasher.write_usize(items.len());
            for item in items {
                hash_json_value(item, hasher);
            }
        }
        Value::Object(members) => {
            hasher.write_u8(5);
            hasher.write_usize(members.len());
            for (key, nested) in members {
                key.hash(hasher);
                hash_json_value(nested, hasher);
            }
        }
    }
}

/// Validate an instance against a JSON schema and return all issues.
pub fn validate_instance(
    schema: &Value,
    instance: &Value,
) -> Result<Vec<ValidationIssue>, SpecMockCoreError> {
    let validator = get_or_compile_validator(schema)?;

    let mut issues = Vec::new();
    for error in validator.iter_errors(instance) {
        let schema_pointer = error.schema_path().as_str().to_owned();
        issues.push(ValidationIssue {
            instance_pointer: error.instance_path().as_str().to_owned(),
            keyword: keyword_from_schema_path(&schema_pointer),
            schema_pointer,
            message: error.to_string(),
        });
    }

    Ok(issues)
}

fn get_or_compile_validator(schema: &Value) -> Result<Arc<Validator>, SpecMockCoreError> {
    let cache_key = hash_value(schema);

    if let Some(cached) =
        VALIDATOR_CACHE.read_sync(&cache_key, |_, validator| Arc::clone(validator))
    {
        return Ok(cached);
    }

    let compiled = Arc::new(validator_for(schema).map_err(|error| {
        SpecMockCoreError::Schema(format!("{error} (schema_path={})", error.schema_path().as_str()))
    })?);

    match VALIDATOR_CACHE.insert_sync(cache_key, Arc::clone(&compiled)) {
        Ok(()) => {
            trim_validator_cache_if_needed();
            Ok(compiled)
        }
        Err((_key, _value)) => VALIDATOR_CACHE
            .read_sync(&cache_key, |_, validator| Arc::clone(validator))
            .ok_or_else(|| {
                SpecMockCoreError::Schema(
                    "validator cache insertion race: validator missing after duplicate insert"
                        .to_owned(),
                )
            }),
    }
}

fn trim_validator_cache_if_needed() {
    let max_entries = VALIDATOR_CACHE_MAX_ENTRIES.load(Ordering::Relaxed).max(1);
    while VALIDATOR_CACHE.len() > max_entries {
        let mut key_to_remove = None::<u64>;
        VALIDATOR_CACHE.iter_sync(|key, _value| {
            key_to_remove = Some(*key);
            false
        });
        if let Some(key) = key_to_remove {
            let _removed = VALIDATOR_CACHE.remove_sync(&key);
        } else {
            break;
        }
    }
}

#[cfg(test)]
pub(crate) fn clear_validator_cache_for_tests() {
    VALIDATOR_CACHE.clear_sync();
}

#[cfg(test)]
pub(crate) fn cached_validator_count_for_tests() -> usize {
    VALIDATOR_CACHE.len()
}

#[cfg(test)]
pub(crate) fn cached_validator_address_for_tests(schema: &Value) -> Option<usize> {
    let cache_key = hash_value(schema);
    VALIDATOR_CACHE.read_sync(&cache_key, |_, validator| Arc::as_ptr(validator) as usize)
}

#[cfg(test)]
pub(crate) fn set_validator_cache_max_for_tests(max_entries: usize) -> usize {
    VALIDATOR_CACHE_MAX_ENTRIES.swap(max_entries.max(1), Ordering::Relaxed)
}

fn keyword_from_schema_path(schema_path: &str) -> String {
    schema_path
        .rsplit('/')
        .find(|part| !part.is_empty())
        .map_or_else(|| "unknown".to_owned(), ToOwned::to_owned)
}

#[cfg(test)]
mod tests {
    use std::sync::{LazyLock, Mutex, MutexGuard};

    use serde_json::json;

    use super::{
        cached_validator_address_for_tests, cached_validator_count_for_tests,
        clear_validator_cache_for_tests, set_validator_cache_max_for_tests, validate_instance,
    };

    static VALIDATOR_CACHE_TEST_GUARD: LazyLock<Mutex<()>> = LazyLock::new(Mutex::default);

    /// RAII guard that ensures proper cleanup of validator cache state.
    struct CacheTestGuard {
        _lock: MutexGuard<'static, ()>,
        old_max: usize,
    }

    impl CacheTestGuard {
        fn new() -> Self {
            let lock = VALIDATOR_CACHE_TEST_GUARD.lock().unwrap_or_else(|poisoned| {
                // If the lock is poisoned, clear the cache to ensure a clean state
                clear_validator_cache_for_tests();
                poisoned.into_inner()
            });
            let old_max = set_validator_cache_max_for_tests(4096);
            clear_validator_cache_for_tests();
            Self { _lock: lock, old_max }
        }
    }

    impl Drop for CacheTestGuard {
        fn drop(&mut self) {
            // Restore original cache max and clear cache for next test
            set_validator_cache_max_for_tests(self.old_max);
            clear_validator_cache_for_tests();
        }
    }

    #[test]
    fn validator_cache_reuses_compiled_schema() {
        let _guard = CacheTestGuard::new();

        let schema = json!({
            "type": "object",
            "required": ["id"],
            "properties": {
                "id": {"type": "integer"}
            }
        });
        let instance = json!({"id": 7});

        assert!(
            cached_validator_address_for_tests(&schema).is_none(),
            "cache should start empty for schema key"
        );

        let first = validate_instance(&schema, &instance);
        assert!(first.is_ok(), "first validation should succeed");
        let first_address = cached_validator_address_for_tests(&schema)
            .expect("schema validator should be present in cache after first validation");

        let second = validate_instance(&schema, &instance);
        assert!(second.is_ok(), "second validation should succeed");
        let second_address = cached_validator_address_for_tests(&schema)
            .expect("schema validator should remain cached after second validation");
        assert_eq!(
            second_address, first_address,
            "second validation should reuse cached validator instance"
        );
    }

    #[test]
    fn validator_cache_respects_max_entries() {
        let _guard = CacheTestGuard::new();
        set_validator_cache_max_for_tests(2);

        let schema_a = json!({"type":"object","properties":{"id":{"type":"integer","minimum":1}}});
        let schema_b = json!({"type":"object","properties":{"id":{"type":"integer","minimum":2}}});
        let schema_c = json!({"type":"object","properties":{"id":{"type":"integer","minimum":3}}});
        let instance = json!({"id": 7});

        let _a = validate_instance(&schema_a, &instance).expect("schema_a should validate");
        let _b = validate_instance(&schema_b, &instance).expect("schema_b should validate");
        let _c = validate_instance(&schema_c, &instance).expect("schema_c should validate");

        assert!(
            cached_validator_count_for_tests() <= 2,
            "validator cache should trim to max entries"
        );
    }

    #[test]
    fn validator_cache_key_ignores_member_ordering() {
        let _guard = CacheTestGuard::new();

        let first = json!({
            "type": "object",
            "required": ["id", "name"],
            "properties": {
                "id": {"type": "integer"},
                "name": {"type": "string"}
            }
        });
        let reordered = json!({
            "properties": {
                "name": {"type": "string"},
                "id": {"type": "integer"}
            },
            "required": ["id", "name"],
            "type": "object"
        });
        let instance = json!({"id": 1, "name": "a"});

        let _ = validate_instance(&first, &instance).expect("first should validate");
        let first_address =
            cached_validator_address_for_tests(&first).expect("first validator should be cached");

        validate_instance(&reordered, &instance).expect("reordered should validate");
        let reordered_address = cached_validator_address_for_tests(&reordered)
            .expect("reordered schema should resolve to the cached validator");

        assert_eq!(
            reordered_address, first_address,
            "member ordering must not change the validator cache key"
        );
    }

    #[test]
    fn validator_cache_key_distinguishes_structurally_different_schemas() {
        let _guard = CacheTestGuard::new();

        let base = json!({"type": "object", "properties": {"id": {"type": "integer"}}});
        // Same members, different nesting: length prefixing must keep keys apart.
        let restructured =
            json!({"type": "object", "properties": {"id": {"type": "integer", "minimum": 1}}});
        let duplicate =
            json!({"type": "object", "properties": {"id": {"type": "integer", "minimum": 1}}});
        let instance = json!({"id": 5});

        let _ = validate_instance(&base, &instance).expect("base should validate");
        let _ = validate_instance(&restructured, &instance).expect("restructured should validate");
        let _ = validate_instance(&duplicate, &instance).expect("duplicate should validate");

        let base_address = cached_validator_address_for_tests(&base).expect("base cached");
        let restructured_address =
            cached_validator_address_for_tests(&restructured).expect("restructured cached");
        let duplicate_address =
            cached_validator_address_for_tests(&duplicate).expect("duplicate cached");

        assert_ne!(
            base_address, restructured_address,
            "schemas with different constraints must not share a validator"
        );
        assert_eq!(
            restructured_address, duplicate_address,
            "identical schemas must share one validator"
        );

        let issues = validate_instance(&restructured, &json!({"id": 0}))
            .expect("restructured schema should validate");
        assert!(
            !issues.is_empty(),
            "the minimum constraint of the restructured schema must still be enforced"
        );
    }

    #[test]
    fn validator_cache_key_handles_nested_and_empty_containers() {
        let _guard = CacheTestGuard::new();

        let nested = json!({
            "type": "object",
            "properties": {
                "items": {"type": "array", "items": {"type": "array", "items": {"type": "string"}}},
                "meta": {"type": "object"},
                "nothing": {"type": "null"},
                "flag": {"type": "boolean"},
                "score": {"type": "number", "multipleOf": 0.5}
            }
        });

        validate_instance(&nested, &json!({"items": [["a"]], "flag": true, "score": 1.5}))
            .expect("nested schema should compile");

        assert!(
            cached_validator_address_for_tests(&nested).is_some(),
            "deeply nested schemas must still be cached"
        );
    }
}
