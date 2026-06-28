//! Deterministic JSON data generator from schema.

use std::collections::BTreeSet;

use rand::{RngExt, SeedableRng};
use rand_chacha::ChaCha8Rng;
use serde_json::{Map, Value};

use crate::{error::SpecMockCoreError, schema};

/// Generate a deterministic JSON value from schema.
///
/// Priority:
/// 1. `example`
/// 2. first value in `examples`
/// 3. `default`
/// 4. schema-based synthetic value
pub fn generate_json_value(schema: &Value, seed: u64) -> Result<Value, SpecMockCoreError> {
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    generate_with_rng(schema, &mut rng, 0)
}

fn generate_with_rng(
    schema: &Value,
    rng: &mut ChaCha8Rng,
    depth: usize,
) -> Result<Value, SpecMockCoreError> {
    if let Some(example) = schema.get("example") {
        return Ok(example.clone());
    }

    if let Some(examples) = schema.get("examples").and_then(Value::as_array) &&
        let Some(first) = examples.first()
    {
        return Ok(first.clone());
    }

    if let Some(default) = schema.get("default") {
        return Ok(default.clone());
    }

    if depth > 32 {
        return Err(SpecMockCoreError::Faker("maximum schema recursion depth reached".to_owned()));
    }

    // Handle oneOf / anyOf with optional discriminator support.
    if let Some(variants) =
        schema.get("oneOf").or_else(|| schema.get("anyOf")).and_then(Value::as_array) &&
        let Some(first) = variants.first()
    {
        let mut value = generate_with_rng(first, rng, depth + 1)?;
        if let Some(disc) = schema::extract_discriminator(schema) &&
            let Value::Object(ref mut obj) = value
        {
            let disc_value =
                disc.mapping.keys().next().cloned().unwrap_or_else(|| "variant_0".to_owned());
            obj.insert(disc.property_name, Value::String(disc_value));
        }
        return Ok(value);
    }
    if let Some(all_of) = schema.get("allOf").and_then(Value::as_array) {
        let mut merged = Value::Object(Map::new());
        for item in all_of {
            let generated = generate_with_rng(item, rng, depth + 1)?;
            merge_values(&mut merged, generated);
        }
        return Ok(merged);
    }

    if let Some(enum_values) = schema.get("enum").and_then(Value::as_array) &&
        let Some(selected) = enum_values.first()
    {
        return Ok(selected.clone());
    }
    if let Some(const_value) = schema.get("const") {
        return Ok(const_value.clone());
    }

    let type_hint = schema_type(schema);
    match type_hint.as_deref() {
        Some("object") => generate_object(schema, rng, depth + 1),
        Some("array") => generate_array(schema, rng, depth + 1),
        Some("integer") => Ok(generate_integer(schema, rng)),
        Some("number") => Ok(generate_number(schema, rng)),
        Some("boolean") => Ok(Value::Bool(rng.random_bool(0.5))),
        Some("null") => Ok(Value::Null),
        Some("string") => Ok(generate_string(schema, rng)),
        Some(other) => Err(SpecMockCoreError::Faker(format!("unsupported schema type: {other}"))),
        None => Ok(Value::Object(Map::new())),
    }
}

fn schema_type(schema: &Value) -> Option<String> {
    if let Some(single) = schema.get("type").and_then(Value::as_str) {
        return Some(single.to_owned());
    }

    let maybe_array = schema.get("type").and_then(Value::as_array)?;
    for item in maybe_array {
        if let Some(name) = item.as_str() &&
            name != "null"
        {
            return Some(name.to_owned());
        }
    }

    Some("null".to_owned())
}

fn generate_object(
    schema: &Value,
    rng: &mut ChaCha8Rng,
    depth: usize,
) -> Result<Value, SpecMockCoreError> {
    let mut object = Map::new();

    let required = required_set(schema);
    let properties =
        schema.get("properties").and_then(Value::as_object).cloned().unwrap_or_default();

    for (name, property_schema) in &properties {
        if required.contains(name) || rng.random_bool(0.35) {
            let value = generate_with_rng(property_schema, rng, depth)?;
            object.insert(name.clone(), value);
        }
    }

    // Handle additionalProperties.
    if let Some(additional) = schema.get("additionalProperties") {
        match additional {
            Value::Bool(false) => { /* no additional properties */ }
            Value::Bool(true) => {
                object.insert("extra_0".to_owned(), Value::String("mock-extra".to_owned()));
            }
            schema_value if schema_value.is_object() => {
                let count = rng.random_range(1_u32..=2);
                for i in 0..count {
                    let key = format!("extra_{i}");
                    let value = generate_with_rng(schema_value, rng, depth)?;
                    object.insert(key, value);
                }
            }
            _ => {}
        }
    }

    Ok(Value::Object(object))
}

fn required_set(schema: &Value) -> BTreeSet<String> {
    let mut set = BTreeSet::new();
    if let Some(required) = schema.get("required").and_then(Value::as_array) {
        for item in required {
            if let Some(name) = item.as_str() {
                set.insert(name.to_owned());
            }
        }
    }
    set
}

fn generate_array(
    schema: &Value,
    rng: &mut ChaCha8Rng,
    depth: usize,
) -> Result<Value, SpecMockCoreError> {
    let min_items = schema.get("minItems").and_then(Value::as_u64).unwrap_or(1).min(100);
    let max_items = schema
        .get("maxItems")
        .and_then(Value::as_u64)
        .unwrap_or(min_items + 2)
        .min(6)
        .max(min_items);
    let item_count = rng.random_range(min_items..=max_items);

    let item_schema = schema.get("items").cloned().unwrap_or_else(|| {
        Value::Object(
            std::iter::once(("type".to_owned(), Value::String("string".to_owned()))).collect(),
        )
    });

    let mut items = Vec::new();
    for _ in 0..item_count {
        items.push(generate_with_rng(&item_schema, rng, depth)?);
    }

    Ok(Value::Array(items))
}

fn generate_integer(schema: &Value, rng: &mut ChaCha8Rng) -> Value {
    let min = schema.get("minimum").and_then(Value::as_i64).unwrap_or(0);
    let max = schema.get("maximum").and_then(Value::as_i64).unwrap_or(min.saturating_add(100));
    let bounded_max = max.max(min);
    Value::from(rng.random_range(min..=bounded_max))
}

fn generate_number(schema: &Value, rng: &mut ChaCha8Rng) -> Value {
    let min = schema.get("minimum").and_then(Value::as_f64).unwrap_or(0.0);
    let max = schema.get("maximum").and_then(Value::as_f64).unwrap_or(min + 100.0);
    let bounded_max = if max < min { min } else { max };
    let value = rng.random_range(min..=bounded_max);
    Value::from(value)
}

fn generate_string(schema: &Value, rng: &mut ChaCha8Rng) -> Value {
    if let Some(format) = schema.get("format").and_then(Value::as_str) {
        let value = match format {
            "date-time" => "2026-03-01T00:00:00Z".to_owned(),
            "date" => "2026-03-01".to_owned(),
            "time" => "12:00:00".to_owned(),
            "duration" => "P1D".to_owned(),
            "email" => "mock@example.com".to_owned(),
            "uuid" => "00000000-0000-0000-0000-000000000000".to_owned(),
            "uri" | "url" => "https://example.com/mock".to_owned(),
            "uri-reference" => "/mock/path".to_owned(),
            "uri-template" => "https://example.com/{id}".to_owned(),
            "iri" => "https://example.com/路径".to_owned(),
            "iri-reference" => "/路径".to_owned(),
            "hostname" => "mock.example.com".to_owned(),
            "ipv4" => "192.0.2.1".to_owned(),
            "ipv6" => "2001:db8::1".to_owned(),
            "byte" => "bW9jaw==".to_owned(),
            "binary" => "0100110001101111".to_owned(),
            "password" => "mock-p4$$w0rd".to_owned(),
            "json-pointer" => "/mock/path".to_owned(),
            "relative-json-pointer" => "0/mock".to_owned(),
            "regex" => "^[a-z]+$".to_owned(),
            _ => format!("mock-{format}"),
        };
        return Value::String(value);
    }

    if let Some(pattern) = schema.get("pattern").and_then(Value::as_str) &&
        let Some(value) = generate_pattern_string(pattern, rng)
    {
        return Value::String(value);
    }

    let min_length = schema.get("minLength").and_then(Value::as_u64).unwrap_or(1).min(10000);
    let max_length = schema
        .get("maxLength")
        .and_then(Value::as_u64)
        .unwrap_or(min_length + 8)
        .min(10000)
        .max(min_length);
    let len = rng.random_range(min_length..=max_length);
    let generated: String = (0..len)
        .map(|_| {
            let code = rng.random_range(b'a'..=b'z');
            char::from(code)
        })
        .collect();
    Value::String(generated)
}

/// Attempt to generate a string matching the given regex pattern.
///
/// Returns `None` (and logs a warning) when the pattern cannot be compiled,
/// allowing the caller to fall back to generic alphanumeric generation.
fn generate_pattern_string(pattern: &str, rng: &mut ChaCha8Rng) -> Option<String> {
    const MAX_REPEAT: u32 = 5;

    // Strip anchors — `rand_regex` generates strings and does not support `^`/`$`.
    let stripped = pattern.strip_prefix('^').unwrap_or(pattern);
    let stripped = stripped.strip_suffix('$').unwrap_or(stripped);

    // Wrap in `(?-u:…)` to force ASCII-only mode. OpenAPI `pattern` follows
    // ECMA-262 where `\d` means `[0-9]`, not Unicode digits.
    let ascii_pattern = format!("(?-u:{stripped})");

    match rand_regex::Regex::compile(&ascii_pattern, MAX_REPEAT) {
        Ok(regex_gen) => Some(rng.sample(&regex_gen)),
        Err(err) => {
            tracing::warn!(
                pattern,
                error = %err,
                "unsupported regex pattern; falling back to alphanumeric generation",
            );
            None
        }
    }
}

fn merge_values(target: &mut Value, source: Value) {
    match (target, source) {
        (Value::Object(target_obj), Value::Object(source_obj)) => {
            for (key, value) in source_obj {
                target_obj.insert(key, value);
            }
        }
        (target_slot, source_value) => *target_slot = source_value,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn faker_uses_default_when_no_example() {
        let schema = json!({
            "type": "string",
            "default": "fallback-value"
        });
        let result = generate_json_value(&schema, 42).unwrap();
        assert_eq!(result, json!("fallback-value"));
    }

    #[test]
    fn faker_prefers_example_over_default() {
        let schema = json!({
            "type": "string",
            "example": "from-example",
            "default": "from-default"
        });
        let result = generate_json_value(&schema, 42).unwrap();
        assert_eq!(result, json!("from-example"));
    }

    #[test]
    fn pattern_uppercase_three_letters() {
        let schema = json!({
            "type": "string",
            "pattern": "^[A-Z]{3}$"
        });
        let result = generate_json_value(&schema, 42).unwrap();
        let s = result.as_str().unwrap();
        assert_eq!(s.len(), 3, "expected 3-char string, got {s:?}");
        assert!(
            s.chars().all(|c| c.is_ascii_uppercase()),
            "expected all uppercase ASCII, got {s:?}"
        );
    }

    #[test]
    fn pattern_date_like_string() {
        let schema = json!({
            "type": "string",
            "pattern": "^\\d{4}-\\d{2}-\\d{2}$"
        });
        let result = generate_json_value(&schema, 42).unwrap();
        let s = result.as_str().unwrap();
        let re = regex_lite::Regex::new(r"^\d{4}-\d{2}-\d{2}$").unwrap();
        assert!(re.is_match(s), "expected date-like pattern, got {s:?}");
    }

    #[test]
    fn pattern_invalid_falls_back_gracefully() {
        // Use an intentionally broken pattern that rand_regex cannot compile.
        let schema = json!({
            "type": "string",
            "pattern": "(?!bad)"
        });
        let result = generate_json_value(&schema, 42).unwrap();
        // Should still produce a string (alphanumeric fallback).
        assert!(result.is_string());
    }

    #[test]
    fn pattern_passes_validation() {
        let schema = json!({
            "type": "string",
            "pattern": "^[A-Z]{3}-\\d{4}$"
        });
        let value = generate_json_value(&schema, 42).unwrap();
        let issues = crate::validate::validate_instance(&schema, &value).unwrap();
        assert!(issues.is_empty(), "validation issues: {issues:?}");
    }

    /// Helper: assert a format string produces the expected value.
    fn assert_format(format: &str, expected: &str) {
        let schema = json!({ "type": "string", "format": format });
        let result = generate_json_value(&schema, 1).unwrap();
        assert_eq!(result, json!(expected), "format={format}");
    }

    #[test]
    fn format_time() {
        assert_format("time", "12:00:00");
    }

    #[test]
    fn format_duration() {
        assert_format("duration", "P1D");
    }

    #[test]
    fn format_uri() {
        assert_format("uri", "https://example.com/mock");
    }

    #[test]
    fn format_url_alias() {
        assert_format("url", "https://example.com/mock");
    }

    #[test]
    fn format_uri_reference() {
        assert_format("uri-reference", "/mock/path");
    }

    #[test]
    fn format_uri_template() {
        assert_format("uri-template", "https://example.com/{id}");
    }

    #[test]
    fn format_iri() {
        assert_format("iri", "https://example.com/路径");
    }

    #[test]
    fn format_iri_reference() {
        assert_format("iri-reference", "/路径");
    }

    #[test]
    fn format_hostname() {
        assert_format("hostname", "mock.example.com");
    }

    #[test]
    fn format_ipv4() {
        assert_format("ipv4", "192.0.2.1");
    }

    #[test]
    fn format_ipv6() {
        assert_format("ipv6", "2001:db8::1");
    }

    #[test]
    fn format_byte() {
        assert_format("byte", "bW9jaw==");
    }

    #[test]
    fn format_binary() {
        assert_format("binary", "0100110001101111");
    }

    #[test]
    fn format_password() {
        assert_format("password", "mock-p4$$w0rd");
    }

    #[test]
    fn format_json_pointer() {
        assert_format("json-pointer", "/mock/path");
    }

    #[test]
    fn format_relative_json_pointer() {
        assert_format("relative-json-pointer", "0/mock");
    }

    #[test]
    fn format_regex() {
        assert_format("regex", "^[a-z]+$");
    }

    #[test]
    fn format_unknown_falls_back() {
        let schema = json!({ "type": "string", "format": "custom-thing" });
        let result = generate_json_value(&schema, 1).unwrap();
        assert_eq!(result, json!("mock-custom-thing"));
    }

    // ── discriminator tests ───────────────────────────────────────────

    #[test]
    fn discriminator_sets_property_on_one_of() {
        let schema = json!({
            "discriminator": {
                "propertyName": "petType",
                "mapping": {
                    "dog": "#/components/schemas/Dog",
                    "cat": "#/components/schemas/Cat"
                }
            },
            "oneOf": [
                {
                    "type": "object",
                    "required": ["petType", "bark"],
                    "properties": {
                        "petType": {"type": "string"},
                        "bark": {"type": "boolean"}
                    }
                },
                {
                    "type": "object",
                    "required": ["petType", "purr"],
                    "properties": {
                        "petType": {"type": "string"},
                        "purr": {"type": "boolean"}
                    }
                }
            ]
        });

        let result = generate_json_value(&schema, 42).unwrap();
        let obj = result.as_object().expect("should be object");
        // The discriminator property should be present and be one of the mapping keys.
        let pet_type = obj.get("petType").expect("petType must exist");
        let pet_str = pet_type.as_str().expect("petType must be string");
        assert!(pet_str == "dog" || pet_str == "cat", "expected dog or cat, got {pet_str}");
    }

    #[test]
    fn discriminator_sets_property_on_any_of() {
        let schema = json!({
            "discriminator": {
                "propertyName": "kind",
                "mapping": {
                    "circle": "#/components/schemas/Circle"
                }
            },
            "anyOf": [
                {
                    "type": "object",
                    "required": ["kind"],
                    "properties": {
                        "kind": {"type": "string"},
                        "radius": {"type": "number"}
                    }
                }
            ]
        });

        let result = generate_json_value(&schema, 7).unwrap();
        let obj = result.as_object().expect("should be object");
        assert_eq!(obj.get("kind").and_then(Value::as_str), Some("circle"));
    }

    #[test]
    fn discriminator_without_mapping_uses_fallback() {
        let schema = json!({
            "discriminator": {
                "propertyName": "type"
            },
            "oneOf": [
                {
                    "type": "object",
                    "properties": {
                        "type": {"type": "string"}
                    }
                }
            ]
        });

        let result = generate_json_value(&schema, 1).unwrap();
        let obj = result.as_object().expect("should be object");
        assert_eq!(obj.get("type").and_then(Value::as_str), Some("variant_0"));
    }

    // ── additionalProperties tests ────────────────────────────────────

    #[test]
    fn additional_properties_true_generates_extra_key() {
        let schema = json!({
            "type": "object",
            "properties": {
                "name": {"type": "string"}
            },
            "required": ["name"],
            "additionalProperties": true
        });

        let result = generate_json_value(&schema, 42).unwrap();
        let obj = result.as_object().expect("should be object");
        assert!(obj.contains_key("name"), "required property missing");
        assert!(obj.contains_key("extra_0"), "extra_0 key missing");
        assert_eq!(obj["extra_0"], json!("mock-extra"));
    }

    #[test]
    fn additional_properties_schema_generates_typed_extras() {
        let schema = json!({
            "type": "object",
            "properties": {
                "id": {"type": "integer"}
            },
            "required": ["id"],
            "additionalProperties": {
                "type": "integer",
                "minimum": 0,
                "maximum": 10
            }
        });

        let result = generate_json_value(&schema, 42).unwrap();
        let obj = result.as_object().expect("should be object");
        assert!(obj.contains_key("id"), "required property missing");
        // Should have 1 or 2 extra keys.
        let extra_count = obj.keys().filter(|k| k.starts_with("extra_")).count();
        assert!((1..=2).contains(&extra_count), "expected 1-2 extra keys, got {extra_count}");
        for key in obj.keys().filter(|k| k.starts_with("extra_")) {
            assert!(obj[key].is_i64() || obj[key].is_u64(), "extra value should be integer");
        }
    }

    #[test]
    fn test_faker_respects_min_items() {
        let schema = json!({
            "type": "array",
            "minItems": 10,
            "items": {"type": "string"}
        });
        let result = generate_json_value(&schema, 42).unwrap();
        assert!(result.is_array());
        assert!(result.as_array().unwrap().len() >= 10);
    }

    #[test]
    fn test_faker_respects_min_length() {
        let schema = json!({
            "type": "string",
            "minLength": 200
        });
        let result = generate_json_value(&schema, 42).unwrap();
        assert!(result.is_string());
        assert!(result.as_str().unwrap().len() >= 200);
    }

    #[test]
    fn additional_properties_false_generates_no_extras() {
        let schema = json!({
            "type": "object",
            "properties": {
                "id": {"type": "integer"}
            },
            "required": ["id"],
            "additionalProperties": false
        });

        let result = generate_json_value(&schema, 42).unwrap();
        let obj = result.as_object().expect("should be object");
        let extra_count = obj.keys().filter(|k| k.starts_with("extra_")).count();
        assert_eq!(extra_count, 0, "should not generate extras when false");
    }

    #[test]
    fn test_integer_faker_no_overflow() {
        let schema = json!({
            "type": "integer",
            "minimum": 9223372036854775707_i64
        });
        let result = generate_json_value(&schema, 42).unwrap();
        assert!(result.is_number());
    }
}
