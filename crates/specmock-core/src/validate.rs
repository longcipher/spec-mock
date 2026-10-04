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
    use std::{
        collections::BTreeSet,
        sync::{LazyLock, Mutex, MutexGuard},
    };

    use serde_json::{Value, json};

    use super::{
        cached_validator_address_for_tests, cached_validator_count_for_tests,
        clear_validator_cache_for_tests, keyword_from_schema_path,
        set_validator_cache_max_for_tests, validate_instance,
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

    #[test]
    fn keyword_is_derived_from_the_schema_path_tail() {
        assert_eq!(keyword_from_schema_path("/properties/id/type"), "type");
        assert_eq!(keyword_from_schema_path("/required"), "required");
        assert_eq!(keyword_from_schema_path("/items/minItems"), "minItems");
    }

    #[test]
    fn keyword_falls_back_to_unknown_for_a_blank_schema_path() {
        assert_eq!(keyword_from_schema_path(""), "unknown");
        assert_eq!(keyword_from_schema_path("/"), "unknown");
        assert_eq!(keyword_from_schema_path("///"), "unknown");
    }

    // ── issue reporting ────────────────────────────────────────────────

    #[test]
    fn type_mismatch_reports_populated_pointers_and_keyword() {
        let schema = json!({"type": "object", "properties": {"id": {"type": "integer"}}});
        let issues = validate_instance(&schema, &json!({"id": "not-a-number"}))
            .expect("schema should compile");

        let issue = issues.first().expect("a type mismatch must produce an issue");
        assert_eq!(issue.instance_pointer, "/id");
        assert_eq!(issue.keyword, "type");
        assert!(issue.schema_pointer.contains("properties"), "got {}", issue.schema_pointer);
        assert!(!issue.message.is_empty(), "every issue must carry a message");
    }

    #[test]
    fn every_failing_keyword_is_reported() {
        let schema = json!({
            "type": "object",
            "required": ["a", "b"],
            "properties": {
                "a": {"type": "integer"},
                "b": {"type": "string"}
            }
        });
        let issues = validate_instance(&schema, &json!({})).expect("schema should compile");
        let keywords: BTreeSet<&str> = issues.iter().map(|issue| issue.keyword.as_str()).collect();
        assert_eq!(
            keywords,
            BTreeSet::from(["required"]),
            "with no properties to type-check, only `required` can fail"
        );
        assert_eq!(issues.len(), 2, "both missing properties are reported separately");
    }

    #[test]
    fn required_and_type_violations_are_reported_together() {
        let schema = json!({
            "type": "object",
            "required": ["b"],
            "properties": {"a": {"type": "integer"}}
        });
        let issues =
            validate_instance(&schema, &json!({"a": "wrong"})).expect("schema should compile");
        let keywords: BTreeSet<&str> = issues.iter().map(|issue| issue.keyword.as_str()).collect();
        assert_eq!(
            keywords,
            BTreeSet::from(["required", "type"]),
            "a missing required property and a wrong property type must both be reported"
        );
    }

    #[test]
    fn root_level_violation_uses_the_empty_instance_pointer() {
        let schema = json!({"type": "string"});
        let issues = validate_instance(&schema, &json!(42)).expect("schema should compile");
        let issue = issues.first().expect("a root violation must produce an issue");
        assert_eq!(issue.instance_pointer, "", "a root violation has no instance path");
    }

    #[test]
    fn array_item_violation_points_at_the_index() {
        let schema = json!({"type": "array", "items": {"type": "integer"}});
        let issues =
            validate_instance(&schema, &json!([1, "two", 3])).expect("schema should compile");
        let issue = issues.first().expect("an item violation must produce an issue");
        assert_eq!(issue.instance_pointer, "/1", "the failing index must be reported");
    }

    #[test]
    fn valid_instance_reports_no_issues() {
        let schema = json!({
            "type": "object",
            "required": ["id"],
            "properties": {"id": {"type": "integer", "minimum": 1}}
        });
        let issues = validate_instance(&schema, &json!({"id": 5})).expect("schema should compile");
        assert!(issues.is_empty(), "a conforming instance must not report issues");
    }

    #[test]
    fn empty_schema_accepts_everything() {
        let issues = validate_instance(&json!({}), &json!({"anything": [1, 2, 3]}))
            .expect("empty schema should compile");
        assert!(issues.is_empty(), "an empty schema must not reject anything");
    }

    #[test]
    fn invalid_schema_is_reported_as_a_compilation_error() {
        let error = validate_instance(&json!({"type": 42}), &json!(null))
            .expect_err("a non-string `type` must fail to compile");
        let message = error.to_string();
        assert!(message.contains("schema compilation failed"), "unexpected error: {error}");
    }

    #[test]
    fn deeply_nested_schema_path_is_preserved() {
        let schema = json!({
            "type": "object",
            "properties": {
                "level1": {
                    "type": "object",
                    "properties": {
                        "level2": {"type": "integer"}
                    }
                }
            }
        });
        let issues = validate_instance(&schema, &json!({"level1": {"level2": "bad"}}))
            .expect("schema should compile");
        let issue = issues.first().expect("a nested violation must produce an issue");
        assert_eq!(issue.instance_pointer, "/level1/level2");
        assert!(issue.schema_pointer.contains("level2"), "got {}", issue.schema_pointer);
    }

    #[test]
    fn pattern_violation_reports_the_pattern_keyword() {
        let schema = json!({"type": "string", "pattern": "^[0-9]+$"});
        let issues = validate_instance(&schema, &json!("abc")).expect("schema should compile");
        let issue = issues.first().expect("a pattern violation must produce an issue");
        assert_eq!(issue.keyword, "pattern");
    }

    // ── hashing ────────────────────────────────────────────────────────

    use super::hash_value;

    #[test]
    fn hash_distinguishes_every_json_variant() {
        let values = [
            Value::Null,
            Value::Bool(true),
            Value::Bool(false),
            Value::from(0),
            Value::from(1),
            Value::from(1.5),
            Value::String(String::new()),
            Value::String("0".to_owned()),
            Value::String("true".to_owned()),
            Value::Array(vec![]),
            Value::Object(serde_json::Map::new()),
        ];

        for (index, left) in values.iter().enumerate() {
            for (other_index, right) in values.iter().enumerate() {
                if index != other_index {
                    assert_ne!(
                        hash_value(left),
                        hash_value(right),
                        "{left} and {right} must not collide"
                    );
                }
            }
        }
    }

    #[test]
    fn hash_ignores_object_member_ordering() {
        let first = json!({"a": 1, "b": [1, 2], "c": {"d": true}});
        let reordered = json!({"c": {"d": true}, "b": [1, 2], "a": 1});
        assert_eq!(hash_value(&first), hash_value(&reordered));
    }

    #[test]
    fn hash_is_sensitive_to_array_ordering() {
        assert_ne!(hash_value(&json!([1, 2])), hash_value(&json!([2, 1])));
    }

    #[test]
    fn hash_is_sensitive_to_member_counts() {
        assert_ne!(hash_value(&json!([])), hash_value(&json!([null])));
        assert_ne!(hash_value(&json!({})), hash_value(&json!({"a": null})));
    }

    #[test]
    fn hash_is_stable_across_calls() {
        let schema = json!({"type": "object", "properties": {"id": {"type": "integer"}}});
        assert_eq!(hash_value(&schema), hash_value(&schema));
    }

    // ── property tests ─────────────────────────────────────────────────

    use proptest::prelude::*;

    /// Recursive strategy for arbitrary JSON values.
    ///
    /// `serde_json::Value` does not implement `proptest::Arbitrary`, so the
    /// generator is assembled from bounded leaf and container shapes.
    fn any_json() -> impl Strategy<Value = Value> {
        let leaf = prop_oneof![
            any::<bool>().prop_map(Value::from),
            any::<i64>().prop_map(Value::from),
            "[ -~]{0,8}".prop_map(Value::from),
        ];
        leaf.prop_recursive(3, 12, 3, |inner| {
            prop_oneof![
                prop::collection::vec(inner.clone(), 0..4).prop_map(Value::Array),
                prop::collection::hash_map("[a-z]{1,6}", inner, 0..4).prop_map(|members| {
                    let map: serde_json::Map<String, Value> = members.into_iter().collect();
                    Value::Object(map)
                }),
            ]
        })
    }

    // `rustfmt` cannot parse the `proptest!` DSL, so this block is left verbatim.

    #[rustfmt::skip]

    proptest! {
        /// Member reordering must never change the cache key, so structurally
        /// equal documents always share one compiled validator.
        #[test]
        fn member_reordering_does_not_change_the_hash(
            members in prop::collection::hash_map(
                "[a-z]{1,5}",
                prop_oneof![any::<i64>().prop_map(Value::from), "[a-z]{0,4}".prop_map(Value::from)],
                1..5,
            )
        ) {
            // `serde_json::Map` stores members in sorted order, so inserting the same
            // entries in reverse yields the identical map — which is exactly the
            // invariant the cache key relies on.
            let entries: Vec<(String, Value)> =
                members.iter().map(|(key, value)| (key.clone(), value.clone())).collect();
            let forward: serde_json::Map<String, Value> =
                entries.iter().cloned().collect();
            let reversed: serde_json::Map<String, Value> =
                entries.iter().rev().cloned().collect();

            prop_assert_eq!(
                hash_value(&Value::Object(forward)),
                hash_value(&Value::Object(reversed))
            );
        }

        /// A one-token change anywhere in the tree must change the key, so two
        /// schemas that differ only in a leaf constraint never share a
        /// validator (which would silently enforce the wrong constraint).
        #[test]
        fn a_single_leaf_change_changes_the_hash(
            base in 0i64..1000,
            other in 0i64..1000,
        ) {
            prop_assume!(base != other);
            let left = json!({"type": "object", "properties": {"id": {"type": "integer", "minimum": base}}});
            let right = json!({"type": "object", "properties": {"id": {"type": "integer", "minimum": other}}});
            prop_assert_ne!(hash_value(&left), hash_value(&right));
        }

        /// Whatever the schema, hashing must be total and never panic.
        #[test]
        fn hashing_never_panics(schema in any_json())
        {
            let first = hash_value(&schema);
            let second = hash_value(&schema);
            prop_assert_eq!(first, second);
        }

        /// `keyword_from_schema_path` must always return a non-empty label.
        #[test]
        fn keyword_is_never_empty(schema_path in "[a-zA-Z0-9#/]{0,24}")
        {
            prop_assert!(!keyword_from_schema_path(&schema_path).is_empty());
        }

        /// Validation over any schema/instance pair either succeeds or fails,
        /// but must never panic — this is the property the whole HTTP and WS
        /// runtime relies on for hostile input.
        #[test]
        fn validation_never_panics(schema in any_json(), instance in any_json())
        {
            let _ = validate_instance(&schema, &instance);
        }

        /// When validation succeeds, every reported issue must be fully
        /// populated so callers can render it into RFC 7807 output.
        #[test]
        fn successful_validation_returns_well_formed_issues(
            schema in any_json(),
            instance in any_json(),
        ) {
            if let Ok(issues) = validate_instance(&schema, &instance) {
                for issue in issues {
                    prop_assert!(!issue.keyword.is_empty());
                    prop_assert!(!issue.message.is_empty());
                }
            }
        }
    }
}
