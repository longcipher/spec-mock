//! Shared contract types, schema validation, and deterministic data generation.

pub mod contract;
pub mod error;
pub mod faker;
pub mod ref_resolver;
pub mod schema;
pub mod validate;

pub use contract::{MockMode, Protocol, ValidationDirection};
pub use error::{PROBLEM_JSON_CONTENT_TYPE, ProblemDetails, SpecMockCoreError, ValidationIssue};

#[cfg(test)]
mod tests {
    use serde_json::json;

    #[test]
    fn faker_generates_value_that_validates() {
        let schema = json!({
            "type": "object",
            "required": ["id", "name"],
            "properties": {
                "id": {"type": "integer", "minimum": 1},
                "name": {"type": "string", "minLength": 1}
            }
        });

        let value =
            crate::faker::generate_json_value(&schema, 7).expect("faker should generate a value");
        let errors = crate::validate::validate_instance(&schema, &value)
            .expect("validator should compile schema");

        assert!(errors.is_empty(), "expected no validation errors: {errors:?}");
    }
}
