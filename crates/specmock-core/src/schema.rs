//! Schema utilities for OpenAPI-specific constructs (discriminator, etc.).

use std::collections::HashMap;

use serde_json::Value;

/// Parsed representation of an OpenAPI `discriminator` object.
#[derive(Debug)]
pub struct Discriminator {
    /// The property name used to distinguish between variants.
    pub property_name: String,
    /// Optional explicit mapping: discriminator value → `$ref` string (or schema name).
    pub mapping: HashMap<String, String>,
}

/// Extract a [`Discriminator`] from a schema that contains a `discriminator` key.
///
/// Returns `None` when the schema does not contain a valid discriminator object
/// (missing `propertyName`, etc.).
pub fn extract_discriminator(schema: &Value) -> Option<Discriminator> {
    let disc = schema.get("discriminator")?.as_object()?;
    let property_name = disc.get("propertyName")?.as_str()?.to_owned();

    let mut mapping = HashMap::new();
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
}
