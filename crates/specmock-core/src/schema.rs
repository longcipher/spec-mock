//! Schema utilities for OpenAPI-specific constructs (discriminator, etc.).

use std::collections::BTreeMap;

use serde_json::Value;

/// Parsed representation of an OpenAPI `discriminator` object.
#[derive(Debug)]
pub struct Discriminator {
    /// The property name used to distinguish between variants.
    pub property_name: String,
    /// Optional explicit mapping: discriminator value → `$ref` string (or schema name).
    ///
    /// Ordered so discriminator-driven variant selection stays deterministic.
    pub mapping: BTreeMap<String, String>,
}

/// Extract a [`Discriminator`] from a schema that contains a `discriminator` key.
///
/// Returns `None` when the schema does not contain a valid discriminator object
/// (missing `propertyName`, etc.).
pub fn extract_discriminator(schema: &Value) -> Option<Discriminator> {
    let disc = schema.get("discriminator")?.as_object()?;
    let property_name = disc.get("propertyName")?.as_str()?.to_owned();

    let mut mapping = BTreeMap::new();
    if let Some(map) = disc.get("mapping").and_then(Value::as_object) {
        for (key, value) in map {
            if let Some(ref_val) = value.as_str() {
                mapping.insert(key.clone(), ref_val.to_owned());
            }
        }
    }

    Some(Discriminator { property_name, mapping })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn extracts_discriminator_with_mapping() {
        let schema = json!({
            "discriminator": {
                "propertyName": "petType",
                "mapping": {
                    "dog": "#/components/schemas/Dog",
                    "cat": "#/components/schemas/Cat"
                }
            },
            "oneOf": [
                {"type": "object", "properties": {"petType": {"type": "string"}}},
                {"type": "object", "properties": {"petType": {"type": "string"}}}
            ]
        });

        let disc = extract_discriminator(&schema).expect("should extract discriminator");
        assert_eq!(disc.property_name, "petType");
        assert_eq!(disc.mapping.len(), 2);
        assert_eq!(disc.mapping["dog"], "#/components/schemas/Dog");
    }

    #[test]
    fn extracts_discriminator_without_mapping() {
        let schema = json!({
            "discriminator": {
                "propertyName": "kind"
            },
            "oneOf": [
                {"type": "object"}
            ]
        });

        let disc = extract_discriminator(&schema).expect("should extract discriminator");
        assert_eq!(disc.property_name, "kind");
        assert!(disc.mapping.is_empty());
    }

    #[test]
    fn returns_none_without_property_name() {
        let schema = json!({
            "discriminator": {
                "mapping": { "a": "#/a" }
            }
        });
        assert!(extract_discriminator(&schema).is_none());
    }

    #[test]
    fn returns_none_without_discriminator() {
        let schema = json!({ "type": "object" });
        assert!(extract_discriminator(&schema).is_none());
    }

    // ── malformed input ────────────────────────────────────────────────

    #[test]
    fn returns_none_when_discriminator_is_not_an_object() {
        for malformed in [
            json!({"discriminator": "petType"}),
            json!({"discriminator": 42}),
            json!({"discriminator": null}),
            json!({"discriminator": ["petType"]}),
        ] {
            assert!(
                extract_discriminator(&malformed).is_none(),
                "non-object discriminator must be ignored: {malformed}"
            );
        }
    }

    #[test]
    fn returns_none_when_property_name_is_not_a_string() {
        for malformed in [json!({"propertyName": 7}), json!({"propertyName": null})] {
            let schema = json!({"discriminator": malformed});
            assert!(
                extract_discriminator(&schema).is_none(),
                "non-string propertyName must be rejected: {schema}"
            );
        }
    }

    #[test]
    fn non_string_mapping_values_are_skipped() {
        let schema = json!({
            "discriminator": {
                "propertyName": "petType",
                "mapping": {
                    "dog": "#/components/schemas/Dog",
                    "cat": 42,
                    "bird": ["#/components/schemas/Bird"],
                    "fish": null
                }
            }
        });

        let disc = extract_discriminator(&schema).expect("should extract discriminator");
        assert_eq!(disc.property_name, "petType");
        assert_eq!(
            disc.mapping.keys().collect::<Vec<_>>(),
            vec!["dog"],
            "only string mapping values can be used as $ref targets"
        );
    }

    #[test]
    fn non_object_mapping_is_ignored() {
        let schema = json!({
            "discriminator": {
                "propertyName": "kind",
                "mapping": ["#/a", "#/b"]
            }
        });

        let disc = extract_discriminator(&schema).expect("should extract discriminator");
        assert_eq!(disc.property_name, "kind");
        assert!(disc.mapping.is_empty(), "a non-object mapping yields no entries");
    }

    #[test]
    fn empty_property_name_is_accepted() {
        let schema = json!({"discriminator": {"propertyName": ""}});
        let disc = extract_discriminator(&schema).expect("should extract discriminator");
        assert_eq!(disc.property_name, "", "an empty name is still a string and must be kept");
    }

    // ── determinism ────────────────────────────────────────────────────

    #[test]
    fn mapping_is_ordered_by_key_regardless_of_input_order() {
        let forward = json!({
            "discriminator": {
                "propertyName": "k",
                "mapping": {"a": "#/A", "b": "#/B", "c": "#/C"}
            }
        });
        let reversed = json!({
            "discriminator": {
                "propertyName": "k",
                "mapping": {"c": "#/C", "b": "#/B", "a": "#/A"}
            }
        });

        let first = extract_discriminator(&forward).expect("forward");
        let second = extract_discriminator(&reversed).expect("reversed");
        assert_eq!(
            first.mapping.keys().collect::<Vec<_>>(),
            vec!["a", "b", "c"],
            "mapping must be sorted so variant selection stays deterministic"
        );
        assert_eq!(first.mapping, second.mapping);
    }

    // ── property tests ─────────────────────────────────────────────────

    use proptest::prelude::*;
    use serde_json::Value;

    /// JSON-pointer-ish key fragment plus a discriminator mapping value.
    fn mapping_entry() -> impl Strategy<Value = (String, String)> {
        ("[a-zA-Z0-9_.-]{1,8}", "#[A-Za-z0-9_./-]{1,16}").prop_map(|(key, target)| (key, target))
    }

    // `rustfmt` cannot parse the `proptest!` DSL, so this block is left verbatim.

    #[rustfmt::skip]

    proptest! {
        /// Whatever subset of string mapping entries survives extraction, the
        /// returned mapping must only contain string keys and string values, and
        /// must be sorted so downstream variant selection is deterministic.
        #[test]
        fn extracted_mapping_is_sorted_and_stringly_typed(entries in prop::collection::vec(mapping_entry(), 0..6))
        {
            let mapping: serde_json::Map<String, Value> = entries
                .iter()
                .map(|(key, target)| (key.clone(), Value::String(target.clone())))
                .collect();
            let schema = json!({"discriminator": {"propertyName": "k", "mapping": mapping}});

            let disc = extract_discriminator(&schema).expect("discriminator should be extractable");
            prop_assert_eq!(disc.property_name.clone(), "k");

            let keys: Vec<&String> = disc.mapping.keys().collect();
            let mut sorted = keys.clone();
            sorted.sort();
            prop_assert_eq!(keys.clone(), sorted, "mapping keys must be sorted");

            for (key, value) in &disc.mapping {
                prop_assert!(!key.is_empty());
                prop_assert!(!value.is_empty(), "every mapping value is a non-empty $ref string");
            }
        }

        /// A discriminator whose `propertyName` is any string is always
        /// extractable, and never panics on arbitrary extra fields.
        #[test]
        fn any_property_name_is_extractable(property_name in ".{1,16}")
        {
            let schema = json!({"discriminator": {"propertyName": property_name}});
            let disc = extract_discriminator(&schema).expect("should extract discriminator");
            prop_assert_eq!(disc.property_name.clone(), property_name);
            prop_assert!(disc.mapping.is_empty());
        }

        /// Documents without a usable `discriminator` never produce a result.
        #[test]
        fn documents_without_discriminator_are_none(flag in any::<bool>())
        {
            let candidates = [
                json!({}),
                json!({"type": "object"}),
                json!({"discriminator": null}),
                json!({"discriminator": []}),
                json!({"discriminator": {"mapping": {"a": "#/a"}}}),
            ];
            let index = usize::from(flag) % candidates.len();
            prop_assert!(extract_discriminator(&candidates[index]).is_none());
        }
    }
}
