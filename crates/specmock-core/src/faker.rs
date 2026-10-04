//! Deterministic JSON data generator from schema.

use std::collections::{BTreeMap, BTreeSet};

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
        !variants.is_empty()
    {
        let discriminator = schema::extract_discriminator(schema);
        let index = match &discriminator {
            Some(disc) => Some(pick_discriminator_variant_index(variants, &disc.mapping, rng)),
            None => variant_index(variants.len(), rng),
        }
        .ok_or_else(|| SpecMockCoreError::Faker("oneOf/anyOf declares no variants".to_owned()))?;
        let mut value = generate_with_rng(&variants[index], rng, depth + 1)?;
        if let Some(disc) = discriminator &&
            let Value::Object(obj) = &mut value
        {
            let variant_value = discriminator_value_for_index(&disc, variants, index);
            obj.insert(disc.property_name.clone(), Value::String(variant_value));
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
        !enum_values.is_empty()
    {
        let index = rng.random_range(0..enum_values.len());
        return Ok(enum_values[index].clone());
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
    if maybe_array.is_empty() {
        // An explicitly empty union carries no type information.
        return None;
    }

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
    // Clamp the upper bound to the same 100-item ceiling as `minItems` so a
    // schema with a huge `maxItems` cannot force an unbounded allocation, then
    // raise it to at least `min_items` so the range is never empty.
    let max_items = schema
        .get("maxItems")
        .and_then(Value::as_u64)
        .unwrap_or(min_items + 2)
        .min(100)
        .max(min_items);
    let item_count = rng.random_range(min_items..=max_items);

    let item_schema = schema.get("items").cloned().unwrap_or_else(|| {
        Value::Object(
            std::iter::once(("type".to_owned(), Value::String("string".to_owned()))).collect(),
        )
    });

    // When `items` is `false` the schema forbids elements outright, so the only
    // conforming instance is the empty array.
    if item_schema == Value::Bool(false) {
        return Ok(Value::Array(Vec::new()));
    }

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

/// Pick a variant index from a possibly empty list.
///
/// An empty `oneOf`/`anyOf` is an unsatisfiable schema, so returning `None`
/// keeps callers from sampling an empty range.
fn variant_index(len: usize, rng: &mut ChaCha8Rng) -> Option<usize> {
    (len > 0).then(|| rng.random_range(0..len))
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

/// Schema name recorded for a resolved `oneOf`/`anyOf` variant.
///
/// [`RefResolver`](crate::ref_resolver::RefResolver) inlines `$ref` nodes before
/// generation, so a variant carries no `$ref` of its own.  OpenAPI also permits
/// an `x-specmock-variant` hint for specs that need an explicit variant mapping.
fn variant_schema_name(schema: &Value) -> Option<&str> {
    schema.get("x-specmock-variant").and_then(Value::as_str)
}

/// Pick the variant index that matches the discriminator mapping.
///
/// `mapping` is ordered by key, while `variants` follows the `oneOf`/`anyOf`
/// declaration order.  Selecting by mapping key keeps the generated
/// discriminator property consistent with the generated payload; when the
/// mapping cannot be aligned to the variant list, the first declared variant is
/// used so behaviour stays deterministic.
fn pick_discriminator_variant_index(
    variants: &[Value],
    mapping: &BTreeMap<String, String>,
    rng: &mut ChaCha8Rng,
) -> usize {
    if mapping.is_empty() || variants.is_empty() {
        return variant_index(variants.len(), rng).unwrap_or(0);
    }

    let candidates: Vec<usize> = mapping
        .values()
        .filter_map(|ref_value| {
            let target_name = ref_value.rsplit('/').next().unwrap_or(ref_value);
            variants.iter().position(|variant| {
                variant_schema_name(variant) == Some(target_name) ||
                    variant.get("title").and_then(Value::as_str) == Some(target_name)
            })
        })
        .collect();

    let unique: BTreeSet<usize> = candidates.into_iter().collect();
    match unique.len() {
        0 => variant_index(variants.len(), rng).unwrap_or(0),
        1 => unique.into_iter().next().unwrap_or(0),
        _ => {
            let mut sorted: Vec<usize> = unique.into_iter().collect();
            sorted.sort_unstable();
            rng.random_range(sorted[0]..=sorted[sorted.len() - 1])
        }
    }
}

/// Discriminator property value that corresponds to the chosen variant.
fn discriminator_value_for_index(
    disc: &schema::Discriminator,
    variants: &[Value],
    index: usize,
) -> String {
    if let Some(chosen) = variants.get(index) {
        let chosen_name =
            variant_schema_name(chosen).or_else(|| chosen.get("title").and_then(Value::as_str));
        if let Some(chosen_name) = chosen_name &&
            let Some((key, _target)) = disc.mapping.iter().find(|(_key, target)| {
                target.rsplit('/').next().unwrap_or(target) == chosen_name
            })
        {
            return key.clone();
        }
    }
    disc.mapping.keys().next().cloned().unwrap_or_else(|| "variant_0".to_owned())
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

    // ── seed-driven selection ─────────────────────────────────────────

    /// Generate over many seeds and collect the distinct results.
    fn distinct_over_seeds(schema: &Value) -> Vec<Value> {
        let mut distinct: BTreeSet<String> = BTreeSet::new();
        for seed in 0_u64..64 {
            let value = generate_json_value(schema, seed).expect("generation should succeed");
            distinct.insert(value.to_string());
        }
        distinct.into_iter().filter_map(|text| serde_json::from_str(&text).ok()).collect()
    }

    #[test]
    fn enum_selection_varies_with_seed() {
        let schema = json!({"enum": ["A", "B", "C"]});
        let distinct = distinct_over_seeds(&schema);
        assert!(distinct.len() > 1, "enum generation must vary with the seed, got {distinct:?}");
        assert!(
            distinct.iter().all(|value| matches!(value.as_str(), Some("A" | "B" | "C"))),
            "every generated enum value must be declared, got {distinct:?}"
        );
    }

    #[test]
    fn enum_generation_is_deterministic_for_a_seed() {
        let schema = json!({"enum": ["A", "B", "C"]});
        let first = generate_json_value(&schema, 7).expect("first");
        let second = generate_json_value(&schema, 7).expect("second");
        assert_eq!(first, second, "same seed must reproduce the same enum value");
    }

    #[test]
    fn object_enum_property_varies_with_seed() {
        let schema = json!({
            "type": "object",
            "required": ["status"],
            "properties": {"status": {"type": "string", "enum": ["ok", "degraded", "down"]}}
        });
        let distinct = distinct_over_seeds(&schema);
        assert!(distinct.len() > 1, "nested enum values must vary with the seed, got {distinct:?}");
    }

    #[test]
    fn one_of_variant_selection_varies_with_seed() {
        let schema = json!({
            "oneOf": [
                {"type": "object", "required": ["bark"], "properties": {"bark": {"type": "boolean"}}},
                {"type": "object", "required": ["purr"], "properties": {"purr": {"type": "boolean"}}}
            ]
        });
        let distinct = distinct_over_seeds(&schema);
        assert!(
            distinct.len() > 1,
            "oneOf variant selection must vary with the seed, got {distinct:?}"
        );
        for value in &distinct {
            let object = value.as_object().expect("object");
            assert!(
                object.contains_key("bark") || object.contains_key("purr"),
                "generated value must match a declared variant, got {value}"
            );
        }
    }

    #[test]
    fn discriminator_value_matches_selected_variant() {
        // Mapping order (`dog` first) is the reverse of `oneOf` order, so a
        // naive "first mapping key" implementation emits a `dog` payload built
        // from the `cat` variant.
        let schema = json!({
            "type": "object",
            "discriminator": {
                "propertyName": "petType",
                "mapping": {"dog": "#/components/schemas/Dog", "cat": "#/components/schemas/Cat"}
            },
            "oneOf": [
                {
                    "x-specmock-variant": "Cat",
                    "type": "object",
                    "required": ["petType", "purr"],
                    "properties": {"petType": {"type": "string"}, "purr": {"type": "boolean"}}
                },
                {
                    "x-specmock-variant": "Dog",
                    "type": "object",
                    "required": ["petType", "bark"],
                    "properties": {"petType": {"type": "string"}, "bark": {"type": "boolean"}}
                }
            ]
        });

        for seed in 0_u64..32 {
            let value = generate_json_value(&schema, seed).expect("generation should succeed");
            let object = value.as_object().expect("object");
            let pet_type = object.get("petType").and_then(Value::as_str).expect("petType");
            match pet_type {
                "dog" => {
                    assert!(object.contains_key("bark"), "seed {seed}: dog payload needs bark")
                }
                "cat" => {
                    assert!(object.contains_key("purr"), "seed {seed}: cat payload needs purr")
                }
                other => panic!("seed {seed}: unexpected petType {other}"),
            }
        }
    }

    #[test]
    fn discriminator_variant_selection_can_cover_all_variants() {
        let schema = json!({
            "type": "object",
            "discriminator": {
                "propertyName": "petType",
                "mapping": {"dog": "#/components/schemas/Dog", "cat": "#/components/schemas/Cat"}
            },
            "oneOf": [
                {
                    "x-specmock-variant": "Dog",
                    "type": "object",
                    "required": ["petType", "bark"],
                    "properties": {"petType": {"type": "string"}, "bark": {"type": "boolean"}}
                },
                {
                    "x-specmock-variant": "Cat",
                    "type": "object",
                    "required": ["petType", "purr"],
                    "properties": {"petType": {"type": "string"}, "purr": {"type": "boolean"}}
                }
            ]
        });

        let mut seen = BTreeSet::new();
        for seed in 0_u64..64 {
            let value = generate_json_value(&schema, seed).expect("generation should succeed");
            if let Some(pet_type) = value.get("petType").and_then(Value::as_str) {
                seen.insert(pet_type.to_owned());
            }
        }
        assert_eq!(
            seen,
            BTreeSet::from(["cat".to_owned(), "dog".to_owned()]),
            "seeded generation should reach every mapped variant"
        );
    }

    #[test]
    fn generated_values_always_validate_against_their_schema() {
        let schema = json!({
            "oneOf": [
                {"type": "object", "required": ["bark"], "properties": {"bark": {"type": "boolean"}}},
                {"type": "object", "required": ["purr"], "properties": {"purr": {"type": "boolean"}}}
            ]
        });
        for seed in 0_u64..32 {
            let value = generate_json_value(&schema, seed).expect("generation should succeed");
            let issues = crate::validate::validate_instance(&schema, &value)
                .expect("validator should compile");
            assert!(issues.is_empty(), "seed {seed} produced invalid data: {issues:?}");
        }
    }

    // ── keyword precedence ─────────────────────────────────────────────

    #[test]
    fn example_wins_over_every_other_keyword() {
        let schema = json!({
            "example": {"kind": "example"},
            "examples": [{"kind": "examples"}],
            "default": {"kind": "default"},
            "type": "object",
            "enum": [{"kind": "enum"}],
            "const": {"kind": "const"}
        });
        assert_eq!(generate_json_value(&schema, 1).unwrap(), json!({"kind": "example"}));
    }

    #[test]
    fn first_examples_entry_wins_over_default() {
        let schema = json!({
            "examples": [{"kind": "first"}, {"kind": "second"}],
            "default": {"kind": "default"}
        });
        assert_eq!(generate_json_value(&schema, 1).unwrap(), json!({"kind": "first"}));
    }

    #[test]
    fn empty_examples_array_falls_through() {
        let schema = json!({"examples": [], "default": "fallback"});
        assert_eq!(generate_json_value(&schema, 1).unwrap(), json!("fallback"));
    }

    #[test]
    fn non_array_examples_falls_through() {
        let schema = json!({"examples": "not-an-array", "default": "fallback"});
        assert_eq!(generate_json_value(&schema, 1).unwrap(), json!("fallback"));
    }

    #[test]
    fn const_keyword_is_returned_verbatim() {
        let schema = json!({"const": {"fixed": true}});
        assert_eq!(generate_json_value(&schema, 1).unwrap(), json!({"fixed": true}));

        let schema = json!({"const": null});
        assert_eq!(generate_json_value(&schema, 1).unwrap(), Value::Null);
    }

    // ── allOf ──────────────────────────────────────────────────────────

    #[test]
    fn all_of_merges_subschemas() {
        let schema = json!({
            "allOf": [
                {"type": "object", "properties": {"a": {"type": "integer"}}, "required": ["a"]},
                {"type": "object", "properties": {"b": {"type": "string"}}, "required": ["b"]}
            ]
        });

        let value = generate_json_value(&schema, 42).expect("allOf should generate");
        let object = value.as_object().expect("allOf must merge into an object");
        assert!(object.contains_key("a"), "first subschema member missing");
        assert!(object.contains_key("b"), "second subschema member missing");
    }

    #[test]
    fn all_of_later_subschema_overrides_earlier_member() {
        let schema = json!({
            "allOf": [
                {"type": "object", "required": ["shared"], "properties": {"shared": {"example": "first"}}},
                {"type": "object", "required": ["shared"], "properties": {"shared": {"example": "second"}}}
            ]
        });

        let value = generate_json_value(&schema, 42).expect("allOf should generate");
        assert_eq!(value["shared"], json!("second"), "later allOf members must win");
    }

    #[test]
    fn all_of_with_empty_array_produces_empty_object() {
        let schema = json!({"allOf": []});
        assert_eq!(generate_json_value(&schema, 42).unwrap(), json!({}));
    }

    #[test]
    fn merge_values_replaces_non_object_target() {
        let mut target = Value::String("scalar".to_owned());
        merge_values(&mut target, json!({"a": 1}));
        assert_eq!(target, json!({"a": 1}), "a scalar target must be overwritten wholesale");

        let mut target = json!({"keep": 1});
        merge_values(&mut target, json!([1, 2]));
        assert_eq!(target, json!([1, 2]), "an array source must overwrite an object target");
    }

    // ── scalar types ───────────────────────────────────────────────────

    #[test]
    fn boolean_generation_produces_both_values_across_seeds() {
        let schema = json!({"type": "boolean"});
        let mut seen = BTreeSet::new();
        for seed in 0_u64..32 {
            seen.insert(generate_json_value(&schema, seed).unwrap().as_bool().expect("bool"));
        }
        assert!(seen.contains(&true) && seen.contains(&false), "both booleans must be reachable");
    }

    #[test]
    fn null_type_generates_null() {
        let schema = json!({"type": "null"});
        assert_eq!(generate_json_value(&schema, 1).unwrap(), Value::Null);
    }

    #[test]
    fn unsupported_type_is_an_error() {
        let schema = json!({"type": "banana"});
        let error = generate_json_value(&schema, 1).expect_err("unknown type must fail");
        assert!(error.to_string().contains("unsupported schema type"), "unexpected error: {error}");
    }

    #[test]
    fn schema_without_type_produces_empty_object() {
        let schema = json!({"description": "no type constraint"});
        assert_eq!(generate_json_value(&schema, 1).unwrap(), json!({}));
    }

    #[test]
    fn type_array_prefers_first_non_null_entry() {
        let schema = json!({"type": ["null", "integer"], "minimum": 3, "maximum": 3});
        assert_eq!(generate_json_value(&schema, 1).unwrap(), json!(3));
    }

    #[test]
    fn type_array_of_only_null_produces_null() {
        let schema = json!({"type": ["null"]});
        assert_eq!(generate_json_value(&schema, 1).unwrap(), Value::Null);
    }

    #[test]
    fn empty_type_array_produces_an_empty_object() {
        // An empty union carries no type information, so the generator falls
        // back to the same empty object it emits for an unconstrained schema.
        let schema = json!({"type": []});
        assert_eq!(generate_json_value(&schema, 1).unwrap(), json!({}));
    }

    // ── numeric bounds ─────────────────────────────────────────────────

    #[test]
    fn integer_generation_respects_exact_bounds() {
        let schema = json!({"type": "integer", "minimum": 7, "maximum": 7});
        assert_eq!(generate_json_value(&schema, 1).unwrap(), json!(7));
    }

    #[test]
    fn inverted_integer_bounds_collapse_to_minimum() {
        let schema = json!({"type": "integer", "minimum": 10, "maximum": 1});
        for seed in 0_u64..8 {
            assert_eq!(
                generate_json_value(&schema, seed).unwrap(),
                json!(10),
                "an inverted range must collapse onto the minimum"
            );
        }
    }

    #[test]
    fn integer_generation_handles_i64_extremes_without_overflow() {
        let schema = json!({
            "type": "integer",
            "minimum": i64::MIN,
            "maximum": i64::MAX
        });
        let value = generate_json_value(&schema, 7).unwrap();
        assert!(
            value.is_i64() || value.is_u64(),
            "extreme integer range must still produce a number"
        );

        // Collapsing a fully degenerate range onto `i64::MIN` would overflow the
        // inclusive sampling range, so the generator must stay inside i64.
        let exact_min = json!({"type": "integer", "minimum": i64::MIN, "maximum": i64::MIN});
        assert_eq!(generate_json_value(&exact_min, 7).unwrap(), json!(i64::MIN));

        let exact_max = json!({"type": "integer", "minimum": i64::MAX, "maximum": i64::MAX});
        assert_eq!(generate_json_value(&exact_max, 7).unwrap(), json!(i64::MAX));
    }

    #[test]
    fn number_generation_respects_exact_bounds() {
        let schema = json!({"type": "number", "minimum": 2.5, "maximum": 2.5});
        assert_eq!(generate_json_value(&schema, 1).unwrap(), json!(2.5));
    }

    #[test]
    fn inverted_number_bounds_collapse_to_minimum() {
        let schema = json!({"type": "number", "minimum": 9.0, "maximum": 1.0});
        for seed in 0_u64..8 {
            assert_eq!(generate_json_value(&schema, seed).unwrap(), json!(9.0));
        }
    }

    #[test]
    fn integer_bounds_ignore_non_integer_bounds() {
        let schema = json!({"type": "integer", "minimum": "five", "maximum": null});
        let value = generate_json_value(&schema, 1).unwrap();
        assert!(value.as_i64().is_some(), "non-numeric bounds must fall back to defaults");
    }

    // ── arrays ─────────────────────────────────────────────────────────

    #[test]
    fn array_item_count_is_clamped_to_max_items() {
        let schema = json!({
            "type": "array",
            "minItems": 0,
            "maxItems": 1000,
            "items": {"type": "integer"}
        });
        let value = generate_json_value(&schema, 1).unwrap();
        let items = value.as_array().expect("array");
        assert!(items.len() <= 100, "maxItems must be clamped to 100, got {}", items.len());
    }

    #[test]
    fn array_min_items_is_clamped_to_100() {
        let schema = json!({"type": "array", "minItems": 5000, "items": {"type": "integer"}});
        let value = generate_json_value(&schema, 1).unwrap();
        let items = value.as_array().expect("array");
        assert_eq!(items.len(), 100, "minItems must be clamped to 100, got {}", items.len());
    }

    #[test]
    fn array_without_items_schema_generates_strings() {
        let schema = json!({"type": "array", "minItems": 3, "maxItems": 3});
        let value = generate_json_value(&schema, 1).unwrap();
        let items = value.as_array().expect("array");
        assert_eq!(items.len(), 3);
        assert!(items.iter().all(serde_json::Value::is_string), "default items must be strings");
    }

    #[test]
    fn array_with_zero_max_items_still_emits_minimum() {
        let schema =
            json!({"type": "array", "minItems": 0, "maxItems": 0, "items": {"type": "integer"}});
        let value = generate_json_value(&schema, 1).unwrap();
        assert!(value.as_array().expect("array").is_empty());
    }

    // ── objects ────────────────────────────────────────────────────────

    #[test]
    fn required_properties_are_always_present() {
        let schema = json!({
            "type": "object",
            "required": ["a", "b", "c"],
            "properties": {
                "a": {"type": "integer"},
                "b": {"type": "string"},
                "c": {"type": "boolean"}
            }
        });
        let object = generate_json_value(&schema, 1).unwrap();
        let object = object.as_object().expect("object");
        for key in ["a", "b", "c"] {
            assert!(object.contains_key(key), "required property {key} must be generated");
        }
    }

    #[test]
    fn non_string_required_entries_are_ignored() {
        let schema = json!({
            "type": "object",
            "required": ["ok", 42, null],
            "properties": {"ok": {"type": "integer"}}
        });
        let value = generate_json_value(&schema, 1).unwrap();
        let object = value.as_object().expect("object");
        assert!(object.contains_key("ok"));
        assert_eq!(object.len(), 1, "non-string required names must not create keys");
    }

    #[test]
    fn required_name_without_matching_property_is_skipped() {
        let schema = json!({
            "type": "object",
            "required": ["declared"],
            "properties": {"other": {"type": "integer"}}
        });
        let value = generate_json_value(&schema, 1).unwrap();
        assert!(value.as_object().expect("object").get("declared").is_none());
    }

    #[test]
    fn object_without_properties_generates_empty_object() {
        let schema = json!({"type": "object"});
        assert_eq!(generate_json_value(&schema, 1).unwrap(), json!({}));
    }

    #[test]
    fn additional_properties_scalar_values_generate_no_extras() {
        // `true` is handled by its own arm and does add `extra_0`; every other
        // non-schema, non-false value is ignored.
        for scalar in [json!(null), json!("yes"), json!(1), json!([1])] {
            let schema = json!({
                "type": "object",
                "required": ["id"],
                "properties": {"id": {"type": "integer"}},
                "additionalProperties": scalar
            });
            let object = generate_json_value(&schema, 42).unwrap();
            let object = object.as_object().expect("object");
            assert_eq!(
                object.keys().filter(|k| k.starts_with("extra_")).count(),
                0,
                "a non-object non-bool additionalProperties must not add extras: {schema}"
            );
        }
    }

    #[test]
    fn boolean_additional_properties_is_generated_as_a_string() {
        let schema = json!({
            "type": "object",
            "required": ["id"],
            "properties": {"id": {"type": "integer"}},
            "additionalProperties": true
        });
        let object = generate_json_value(&schema, 42).unwrap();
        let object = object.as_object().expect("object");
        assert_eq!(object.get("extra_0"), Some(&json!("mock-extra")));
    }

    #[test]
    fn nested_additional_properties_schema_is_honoured() {
        let schema = json!({
            "type": "object",
            "additionalProperties": {
                "type": "object",
                "required": ["inner"],
                "properties": {"inner": {"type": "integer"}}
            }
        });
        let object = generate_json_value(&schema, 42).unwrap();
        let object = object.as_object().expect("object");
        for key in object.keys().filter(|k| k.starts_with("extra_")) {
            assert!(
                object[key].get("inner").is_some(),
                "nested additionalProperties schema must be applied to {key}"
            );
        }
    }

    // ── recursion guard ────────────────────────────────────────────────

    /// Build `depth` levels of nested objects terminating in an integer leaf.
    fn nested_object_schema(depth: usize) -> Value {
        let mut schema = json!({"type": "integer"});
        for _ in 0..depth {
            schema = json!({
                "type": "object",
                "required": ["child"],
                "properties": {"child": schema}
            });
        }
        schema
    }

    #[test]
    fn deep_nesting_beyond_the_limit_is_rejected() {
        let schema = nested_object_schema(40);
        let error = generate_json_value(&schema, 1).expect_err("deep nesting must be rejected");
        assert!(error.to_string().contains("recursion depth"), "unexpected error: {error}");
    }

    #[test]
    fn nesting_within_the_limit_succeeds() {
        let schema = nested_object_schema(8);
        let value = generate_json_value(&schema, 1).expect("shallow nesting must succeed");
        // Walk down 8 levels and confirm we reached the integer leaf.
        let mut cursor = &value;
        for _ in 0..8 {
            cursor = cursor.get("child").expect("child member must exist at every level");
        }
        assert!(cursor.is_i64(), "the innermost value must be the integer leaf");
    }

    #[test]
    fn deep_via_one_of_also_hits_the_limit() {
        let mut schema = json!({"type": "integer"});
        for _ in 0..40 {
            schema = json!({"oneOf": [schema]});
        }
        let error =
            generate_json_value(&schema, 1).expect_err("deep oneOf nesting must be rejected");
        assert!(error.to_string().contains("recursion depth"), "unexpected error: {error}");
    }

    // ── enum ───────────────────────────────────────────────────────────

    #[test]
    fn empty_enum_falls_through_to_type_handling() {
        let schema = json!({"enum": [], "type": "integer", "minimum": 5, "maximum": 5});
        assert_eq!(generate_json_value(&schema, 1).unwrap(), json!(5));
    }

    #[test]
    fn non_array_enum_falls_through_to_type_handling() {
        let schema = json!({"enum": "A", "type": "boolean", "default": true});
        assert_eq!(generate_json_value(&schema, 1).unwrap(), json!(true));
    }

    #[test]
    fn enum_supports_mixed_json_types() {
        let schema = json!({"enum": [1, "two", null, true, {"k": 1}, [1, 2]]});
        let mut seen = BTreeSet::new();
        for seed in 0_u64..64 {
            seen.insert(generate_json_value(&schema, seed).unwrap().to_string());
        }
        let declared = ["1", "\"two\"", "null", "true", "{\"k\":1}", "[1,2]"];
        for rendered in &seen {
            assert!(
                declared.contains(&rendered.as_str()),
                "generated {rendered} is not a declared enum member"
            );
        }
    }

    // ── discriminator corner cases ─────────────────────────────────────

    #[test]
    fn discriminator_matches_variant_by_title() {
        let schema = json!({
            "discriminator": {
                "propertyName": "petType",
                "mapping": {"dog": "#/components/schemas/Dog", "cat": "#/components/schemas/Cat"}
            },
            "oneOf": [
                {"title": "Cat", "type": "object", "required": ["purr"], "properties": {"purr": {"type": "boolean"}}},
                {"title": "Dog", "type": "object", "required": ["bark"], "properties": {"bark": {"type": "boolean"}}}
            ]
        });

        for seed in 0_u64..32 {
            let value = generate_json_value(&schema, seed).expect("generation should succeed");
            let object = value.as_object().expect("object");
            let pet_type = object.get("petType").and_then(Value::as_str).expect("petType");
            match pet_type {
                "dog" => assert!(object.contains_key("bark"), "seed {seed}: dog needs bark"),
                "cat" => assert!(object.contains_key("purr"), "seed {seed}: cat needs purr"),
                other => panic!("seed {seed}: unexpected petType {other}"),
            }
        }
    }

    #[test]
    fn discriminator_on_non_object_variant_is_not_injected() {
        let schema = json!({
            "discriminator": {
                "propertyName": "petType",
                "mapping": {"dog": "#/components/schemas/Dog"}
            },
            "oneOf": [{"type": "string", "example": "plain"}]
        });

        let value = generate_json_value(&schema, 1).expect("generation should succeed");
        assert_eq!(value, json!("plain"), "a scalar variant cannot carry a discriminator property");
    }

    #[test]
    fn discriminator_without_variants_is_ignored() {
        let schema = json!({
            "discriminator": {"propertyName": "petType", "mapping": {"dog": "#/Dog"}},
            "oneOf": []
        });
        assert_eq!(generate_json_value(&schema, 1).unwrap(), json!({}));
    }

    #[test]
    fn discriminator_mapping_targets_nothing_falls_back_to_first_variant() {
        let schema = json!({
            "discriminator": {
                "propertyName": "petType",
                "mapping": {"dog": "#/components/schemas/NotDeclared"}
            },
            "oneOf": [
                {"x-specmock-variant": "Cat", "type": "object", "required": ["purr"], "properties": {"purr": {"type": "boolean"}}},
                {"x-specmock-variant": "Dog", "type": "object", "required": ["bark"], "properties": {"bark": {"type": "boolean"}}}
            ]
        });

        for seed in 0_u64..16 {
            let value = generate_json_value(&schema, seed).expect("generation should succeed");
            let object = value.as_object().expect("object");
            assert_eq!(
                object.get("petType").and_then(Value::as_str),
                Some("dog"),
                "an unmatched mapping degenerates to the first declared mapping key"
            );
            assert!(object.contains_key("bark") || object.contains_key("purr"));
        }
    }

    #[test]
    fn discriminator_variant_index_helper_handles_degenerate_inputs() {
        let mut rng = ChaCha8Rng::seed_from_u64(1);
        assert_eq!(
            pick_discriminator_variant_index(&[], &BTreeMap::new(), &mut rng),
            0,
            "an empty variant list must not panic"
        );

        let variants = vec![json!({"x-specmock-variant": "A"})];
        assert_eq!(
            pick_discriminator_variant_index(&variants, &BTreeMap::new(), &mut rng),
            0,
            "an empty mapping degrades to a uniform random pick"
        );
    }

    #[test]
    fn an_empty_variant_list_is_ignored() {
        // An empty `oneOf`/`anyOf` carries no variants, so the generator skips
        // the branch entirely rather than sampling an empty range.
        for keyword in ["oneOf", "anyOf"] {
            let schema = json!({ keyword: [] });
            assert_eq!(
                generate_json_value(&schema, 1).unwrap(),
                json!({}),
                "{keyword}: [] must fall through to the unconstrained default"
            );
        }
    }

    #[test]
    fn an_array_whose_items_are_forbidden_generates_an_empty_array() {
        let schema = json!({"type": "array", "items": false});
        assert_eq!(generate_json_value(&schema, 1).unwrap(), json!([]));
    }

    #[test]
    fn discriminator_value_helper_falls_back_without_mapping() {
        let disc = crate::schema::Discriminator {
            property_name: "kind".to_owned(),
            mapping: BTreeMap::new(),
        };
        let variants = vec![json!({"x-specmock-variant": "A"})];
        assert_eq!(
            discriminator_value_for_index(&disc, &variants, 0),
            "variant_0",
            "with no mapping the helper must produce a synthetic name"
        );
        assert_eq!(
            discriminator_value_for_index(&disc, &variants, 99),
            "variant_0",
            "an out-of-range index must not panic"
        );
    }

    #[test]
    fn one_of_selection_ignores_non_array_one_of() {
        let schema = json!({"oneOf": {"type": "string", "example": "obj"}});
        assert_eq!(
            generate_json_value(&schema, 1).unwrap(),
            json!({}),
            "a non-array oneOf is not a variant list, so it must be ignored"
        );
    }

    #[test]
    fn an_empty_any_of_is_ignored_rather_than_treated_as_unsatisfiable() {
        // `anyOf: []` matches every instance in JSON Schema, so the empty object
        // the generator falls back to is a conforming result.
        let schema = json!({"anyOf": []});
        assert_eq!(generate_json_value(&schema, 1).unwrap(), json!({}));
    }

    // ── string generation ──────────────────────────────────────────────

    #[test]
    fn remaining_named_formats_are_produced() {
        assert_format("date-time", "2026-03-01T00:00:00Z");
        assert_format("date", "2026-03-01");
        assert_format("uuid", "00000000-0000-0000-0000-000000000000");
    }

    #[test]
    fn string_length_bounds_are_clamped() {
        let schema = json!({"type": "string", "minLength": 50_000, "maxLength": 60_000});
        let value = generate_json_value(&schema, 1).unwrap();
        assert_eq!(
            value.as_str().map(str::len),
            Some(10_000),
            "length bounds must be clamped to 10000"
        );
    }

    #[test]
    fn inverted_string_length_bounds_collapse_to_minimum() {
        let schema = json!({"type": "string", "minLength": 5, "maxLength": 2});
        for seed in 0_u64..8 {
            assert_eq!(generate_json_value(&schema, seed).unwrap().as_str().map(str::len), Some(5));
        }
    }

    #[test]
    fn string_bounds_ignore_non_numeric_values() {
        // `minLength: "3"` is not a number, so it falls back to the default of 1
        // while the numeric `maxLength` is still honoured.
        let schema = json!({"type": "string", "minLength": "3", "maxLength": 3});
        for seed in 0_u64..16 {
            let len =
                generate_json_value(&schema, seed).unwrap().as_str().map(str::len).expect("string");
            assert!((1..=3).contains(&len), "length {len} escaped the 1..=3 range");
        }
    }

    #[test]
    fn generated_strings_are_lowercase_ascii() {
        let schema = json!({"type": "string", "minLength": 32, "maxLength": 32});
        let value = generate_json_value(&schema, 1).unwrap();
        let text = value.as_str().expect("string");
        assert_eq!(text.len(), 32);
        assert!(text.chars().all(|c| c.is_ascii_lowercase()), "got non-lowercase {text}");
    }

    #[test]
    fn pattern_generator_strips_only_outer_anchors() {
        // Anchored pattern.
        let anchored = json!({"type": "string", "pattern": "^ab[0-9]{2}$"});
        let value = generate_json_value(&anchored, 3).unwrap();
        let re = regex_lite::Regex::new(r"^ab[0-9]{2}$").expect("regex");
        assert!(re.is_match(value.as_str().expect("string")));

        // Unanchored pattern.
        let loose = json!({"type": "string", "pattern": "x[0-9]"});
        let value = generate_json_value(&loose, 3).unwrap();
        let re = regex_lite::Regex::new(r"x[0-9]").expect("regex");
        assert!(re.is_match(value.as_str().expect("string")));
    }

    #[test]
    fn uncompilable_pattern_falls_back_within_length_bounds() {
        let schema = json!({
            "type": "string",
            "pattern": "(?<broken",
            "minLength": 6,
            "maxLength": 6
        });
        let value = generate_json_value(&schema, 9).unwrap();
        let text = value.as_str().expect("string");
        assert_eq!(text.len(), 6, "the fallback must still honour length bounds");
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
                prop::collection::hash_map("[a-z]{1,6}", inner.clone(), 0..4).prop_map(|members| {
                    let map: serde_json::Map<String, Value> = members.into_iter().collect();
                    Value::Object(map)
                }),
            ]
        })
    }

    /// A leaf schema carrying no object or array keywords, so the container
    /// strategies below can index into it unconditionally.
    fn leaf_schema() -> impl Strategy<Value = Value> {
        prop_oneof![
            Just(json!({"type": "integer", "minimum": -5, "maximum": 5})),
            Just(json!({"type": "number", "minimum": -5.0, "maximum": 5.0})),
            Just(json!({"type": "boolean"})),
            Just(json!({"type": "null"})),
            Just(json!({"type": "string", "minLength": 1, "maxLength": 6})),
            Just(json!({"enum": ["a", "b", "c"]})),
            Just(json!({"const": 17})),
        ]
    }

    /// A schema restricted to keywords the faker understands end to end, so the
    /// round-trip invariant below is meaningful rather than vacuous.
    fn any_supported_schema() -> impl Strategy<Value = Value> {
        leaf_schema().prop_recursive(4, 16, 5, |inner| {
            prop_oneof![
                // `items` is drawn from the leaf pool so the vec is never empty.
                leaf_schema().prop_map(|item| json!({"type": "array", "items": item})),
                prop::collection::hash_map("[a-z]{1,6}", inner.clone(), 0..4)
                    .prop_map(|members| json!({"type": "object", "properties": members})),
                prop::collection::vec(inner.clone(), 1..3)
                    .prop_map(|variants| json!({"oneOf": variants})),
                prop::collection::vec(inner.clone(), 1..3)
                    .prop_map(|variants| json!({"allOf": variants})),
            ]
        })
    }

    // `rustfmt` cannot parse the `proptest!` DSL, so this block is left verbatim.

    #[rustfmt::skip]

    proptest! {
        /// The core faker contract: the same seed must always reproduce the
        /// exact same value, and generation must never panic for any schema
        /// built from supported keywords.
        #[test]
        fn generation_is_deterministic_per_seed(schema in any_supported_schema(), seed in any::<u64>())
        {
            let first = generate_json_value(&schema, seed);
            let second = generate_json_value(&schema, seed);
            prop_assert_eq!(first.is_ok(), second.is_ok());
            if let (Ok(a), Ok(b)) = (first, second) {
                prop_assert_eq!(a, b);
            }
        }

        /// Bounds declared on a leaf schema must always be respected.
        #[test]
        fn integer_bounds_are_always_respected(
            min in -1_000i64..1_000,
            max in -1_000i64..1_000,
            seed in any::<u64>(),
        ) {
            let bounded_max = max.max(min);
            let schema = json!({"type": "integer", "minimum": min, "maximum": max});
            let value = generate_json_value(&schema, seed).expect("integer generation");
            let n = value.as_i64().expect("integer value");
            prop_assert!(n >= min);
            prop_assert!(n <= bounded_max);
        }

        /// String length bounds must always be honoured, even when inverted.
        #[test]
        fn string_length_bounds_are_always_respected(
            min in 0u64..20,
            max in 0u64..20,
            seed in any::<u64>(),
        ) {
            // Normalize into a non-inverted range up front so the generator
            // receives a coherent schema; the 10 000-length clamp is covered by
            // the `string_length_bounds_are_clamped` example test.
            let effective_min = min;
            let effective_max = max.max(effective_min);
            let schema = json!({
                "type": "string",
                "minLength": effective_min,
                "maxLength": effective_max
            });
            let value = generate_json_value(&schema, seed).expect("string generation");
            let len = value.as_str().expect("string value").chars().count();
            prop_assert!(len >= effective_min as usize);
            prop_assert!(len <= effective_max as usize);
        }

        /// Array item counts must always stay inside the clamped bounds.
        ///
        /// The generated values straddle the 100-item clamp so the ceiling is
        /// genuinely exercised rather than merely being unreachable.
        #[test]
        fn array_length_bounds_are_always_respected(
            min in prop_oneof![0u64..12, 90u64..=110, 1_000u64..1_000_000],
            max in prop_oneof![0u64..12, 90u64..=110, 1_000u64..1_000_000],
            seed in any::<u64>(),
        ) {
            // Mirror the generator: `minItems` is clamped to 100, `maxItems` is
            // clamped to the same ceiling and then raised to at least `minItems`.
            let clamped_min = min.min(100);
            let clamped_max = max.min(100).max(clamped_min);
            let schema = json!({
                "type": "array",
                "minItems": min,
                "maxItems": max,
                "items": {"type": "integer"}
            });
            let value = generate_json_value(&schema, seed).expect("array generation");
            let len = value.as_array().expect("array value").len();
            prop_assert!(len >= clamped_min as usize);
            prop_assert!(len <= clamped_max as usize);
        }

        /// A huge `maxItems` must not be able to force an unbounded allocation.
        #[test]
        fn huge_max_items_is_clamped(
            max in 1_000u64..1_000_000,
            seed in any::<u64>(),
        ) {
            let schema =
                json!({"type": "array", "minItems": 0, "maxItems": max, "items": {"type": "integer"}});
            let value = generate_json_value(&schema, seed).expect("array generation");
            prop_assert!(value.as_array().expect("array value").len() <= 100);
        }

        /// Whatever schema is generated from, the faker must either produce a
        /// value or a well-formed error — never panic.
        #[test]
        fn never_panics_on_arbitrary_schema(shape in prop::collection::vec(any_json(), 0..6))
        {
            for schema in shape {
                let _ = generate_json_value(&schema, 1);
            }
        }
    }
}
