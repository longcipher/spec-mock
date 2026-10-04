//! Fuzzes deterministic mock-data generation from hostile schemas.
//!
//! `generate_json_value` walks a schema recursively and consumes keywords that
//! control loop bounds (`minItems`, `minLength`, `minimum`), pattern
//! compilation (`pattern`), and recursion through `oneOf`/`allOf`. A hostile
//! spec can therefore drive the generator into deep recursion, huge allocations,
//! or a regex compile that panics.
//!
//! Invariants:
//! - generation never panics;
//! - the same seed always reproduces the same value (determinism);
//! - a generated value is always finite-size and JSON-serializable;
//! - when generation succeeds, the value respects the scalar bounds the
//!   generator claims to honour.

#![no_main]

use libfuzzer_sys::fuzz_target;
use serde_json::{Value, json};
use specmock_core::faker::generate_json_value;

/// Bound the serialized output size so a finding reports a stack overflow or an
/// allocation blow-up rather than running the machine out of memory.
const MAX_OUTPUT_BYTES: usize = 1 << 20;

fuzz_target!(|data: &[u8]| {
    let split = data.len() / 2;
    let (schema_bytes, seed_bytes) = data.split_at(split);

    let schema: Value = match serde_json::from_slice(schema_bytes) {
        Ok(value) => value,
        Err(_) => json!({ "type": String::from_utf8_lossy(schema_bytes) }),
    };

    let seed = u64::from_le_bytes({
        let mut bytes = [0_u8; 8];
        let len = seed_bytes.len().min(8);
        bytes[..len].copy_from_slice(&seed_bytes[..len]);
        bytes
    });

    let Ok(first) = generate_json_value(&schema, seed) else {
        // Generation is allowed to fail (unsupported type, depth limit, bad
        // pattern) — a failure is not a finding.
        return;
    };

    // Determinism: the same seed must reproduce the same value.
    let Ok(second) = generate_json_value(&schema, seed) else {
        panic!("generation succeeded once and failed for the same schema and seed");
    };
    assert_eq!(first, second, "generation is not deterministic for seed {seed}");

    let Ok(encoded) = serde_json::to_vec(&first) else {
        panic!("generated value is not JSON-serializable: {first}");
    };
    assert!(
        encoded.len() <= MAX_OUTPUT_BYTES,
        "generated {} bytes for a {}-byte schema; generation is not size-bounded",
        encoded.len(),
        schema_bytes.len()
    );

    assert_scalar_bounds(&schema, &first);
});

/// Structural check for the bounds the generator advertises on scalar schemas.
fn assert_scalar_bounds(schema: &Value, generated: &Value) {
    let Some(declared) = schema.get("type").and_then(Value::as_str) else {
        return;
    };

    match (declared, generated) {
        ("integer" | "number", Value::Number(number)) => {
            let Some(value) = number.as_f64() else { return };
            assert!(value.is_finite(), "generated a non-finite number: {value}");

            if let Some(min) = schema.get("minimum").and_then(Value::as_f64) {
                assert!(value >= min.min(f64::MAX), "generated {value} below minimum {min}");
            }
            if let Some(max) = schema.get("maximum").and_then(Value::as_f64) {
                assert!(
                    value <= max.max(schema.get("minimum").and_then(Value::as_f64).unwrap_or(f64::MIN)),
                    "generated {value} above maximum {max}"
                );
            }
        }
        ("string", Value::String(text)) => {
            let len = text.chars().count() as u64;
            let min = schema.get("minLength").and_then(Value::as_u64).unwrap_or(0).min(10_000);
            let max = schema
                .get("maxLength")
                .and_then(Value::as_u64)
                .unwrap_or(min + 8)
                .min(10_000)
                .max(min);
            assert!(
                (min..=max).contains(&len),
                "generated a {len}-character string outside the clamped bounds {min}..={max}"
            );
        }
        ("array", Value::Array(items)) => {
            let min = schema.get("minItems").and_then(Value::as_u64).unwrap_or(1).min(100);
            let max = schema.get("maxItems").and_then(Value::as_u64).unwrap_or(min + 2).min(6).max(min);
            let len = items.len() as u64;
            assert!(
                (min..=max).contains(&len),
                "generated a {len}-element array outside the clamped bounds {min}..={max}"
            );
        }
        ("object" | "boolean" | "null", _) => {}
        _ => {}
    }
}