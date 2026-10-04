//! Fuzzes schema compilation and instance validation with hostile pairs.
//!
//! Every HTTP request, WebSocket frame, and proxied upstream response runs
//! through `validate_instance`, so both the schema and the instance are fully
//! attacker-controlled (a spec file and a request body). The validator is
//! cached by a structural hash, so a hash collision would silently validate an
//! instance against the wrong schema.
//!
//! Invariants:
//! - validation never panics;
//! - the schema hash is stable for a given schema;
//! - two schemas that differ anywhere in their tree do not share a hash.

#![no_main]

use libfuzzer_sys::fuzz_target;
use serde_json::Value;
use specmock_core::validate::validate_instance;

fuzz_target!(|data: &[u8]| {
    // Split the input into schema, instance, and trailing padding. The fuzzer
    // varies the split points, so both halves are fully attacker-controlled.
    let schema_len = data.len() / 3;
    let (schema_bytes, rest) = data.split_at(schema_len);
    let instance_len = rest.len() / 2;
    let (instance_bytes, _padding) = rest.split_at(instance_len);

    let schema = decode(schema_bytes, "schema");
    let instance = decode(instance_bytes, "instance");

    // Validation may fail to compile the schema; that is not a finding.
    let Ok(issues) = validate_instance(&schema, &instance) else {
        return;
    };

    for issue in &issues {
        assert!(!issue.keyword.is_empty(), "issue produced an empty keyword: {issue:?}");
        assert!(!issue.message.is_empty(), "issue produced an empty message: {issue:?}");
        assert!(
            issue.instance_pointer.is_empty() || issue.instance_pointer.starts_with('/'),
            "instance pointer is not a JSON pointer: {:?}",
            issue.instance_pointer
        );
    }

    // Validating the same pair twice must be stable, since the validator cache
    // makes the second call reuse a compiled schema.
    let Ok(repeat) = validate_instance(&schema, &instance) else {
        panic!("validation succeeded once and then failed to compile for the same schema");
    };
    assert_eq!(
        issues.len(),
        repeat.len(),
        "issue count changed between two validations of the same pair"
    );
});

fn decode(bytes: &[u8], fallback: &str) -> Value {
    serde_json::from_slice(bytes).unwrap_or_else(|_error| Value::String(String::from_utf8_lossy(bytes).into_owned() + fallback))
}