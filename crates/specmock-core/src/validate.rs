//! JSON schema validation helpers.

use std::sync::{
    Arc, LazyLock,
    atomic::{AtomicUsize, Ordering},
};

use jsonschema::{Validator, validator_for};
use scc::HashMap;
use serde_json::Value;

use crate::error::{SpecMockCoreError, ValidationIssue};

static VALIDATOR_CACHE: LazyLock<HashMap<String, Arc<Validator>>> = LazyLock::new(HashMap::new);
const DEFAULT_VALIDATOR_CACHE_MAX_ENTRIES: usize = 256;
static VALIDATOR_CACHE_MAX_ENTRIES: AtomicUsize =
    AtomicUsize::new(DEFAULT_VALIDATOR_CACHE_MAX_ENTRIES);

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
    let cache_key = serde_json::to_string(schema).map_err(|error| {
        SpecMockCoreError::Schema(format!("schema cache key serialization failed: {error}"))
    })?;

    if let Some(cached) =
        VALIDATOR_CACHE.read_sync(&cache_key, |_, validator| Arc::clone(validator))
    {
        return Ok(cached);
    }

    let compiled = Arc::new(validator_for(schema).map_err(|error| {
        SpecMockCoreError::Schema(format!("{error} (schema_path={})", error.schema_path().as_str()))
    })?);

    match VALIDATOR_CACHE.insert_sync(cache_key.clone(), Arc::clone(&compiled)) {
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
        let mut key_to_remove = None::<String>;
        VALIDATOR_CACHE.iter_sync(|key, _value| {
            key_to_remove = Some(key.clone());
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
    let cache_key = serde_json::to_string(schema).ok()?;
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
    use std::sync::{LazyLock, Mutex};

    use serde_json::json;

    use super::{
        cached_validator_address_for_tests, cached_validator_count_for_tests,
        clear_validator_cache_for_tests, set_validator_cache_max_for_tests, validate_instance,
    };

    static VALIDATOR_CACHE_TEST_GUARD: LazyLock<Mutex<()>> = LazyLock::new(Mutex::default);

    struct CacheLimitReset(usize);

    impl Drop for CacheLimitReset {
        fn drop(&mut self) {
            let _previous = set_validator_cache_max_for_tests(self.0);
        }
    }

    #[test]
    fn validator_cache_reuses_compiled_schema() {
        let _guard = VALIDATOR_CACHE_TEST_GUARD.lock().unwrap_or_else(|err| err.into_inner());
        let old_max = set_validator_cache_max_for_tests(4096);
        let _reset = CacheLimitReset(old_max);
        clear_validator_cache_for_tests();

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
        let _guard = VALIDATOR_CACHE_TEST_GUARD.lock().unwrap_or_else(|err| err.into_inner());
        clear_validator_cache_for_tests();
        let old_max = set_validator_cache_max_for_tests(2);
        let _reset = CacheLimitReset(old_max);

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
}
