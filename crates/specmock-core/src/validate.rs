//! JSON schema validation helpers.

use jsonschema::validator_for;
use serde_json::Value;

use crate::error::{SpecMockCoreError, ValidationIssue};

/// Validate an instance against a JSON schema and return all issues.
pub fn validate_instance(
    schema: &Value,
    instance: &Value,
) -> Result<Vec<ValidationIssue>, SpecMockCoreError> {
    let validator = validator_for(schema).map_err(|error| {
        SpecMockCoreError::Schema(format!("{error} (schema_path={})", error.schema_path().as_str()))
    })?;

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

fn keyword_from_schema_path(schema_path: &str) -> String {
    schema_path
        .rsplit('/')
        .find(|part| !part.is_empty())
        .map_or_else(|| "unknown".to_owned(), ToOwned::to_owned)
}
