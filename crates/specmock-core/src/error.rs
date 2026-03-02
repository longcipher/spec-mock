//! Error and validation issue models.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Content-Type for RFC 7807 responses.
pub const PROBLEM_JSON_CONTENT_TYPE: &str = "application/problem+json";

/// Standard validation issue item with JSON pointer locations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationIssue {
    /// JSON pointer into instance payload.
    pub instance_pointer: String,
    /// JSON pointer into schema.
    pub schema_pointer: String,
    /// Best-effort keyword.
    pub keyword: String,
    /// Human-readable description.
    pub message: String,
}

/// RFC 7807 Problem Details response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProblemDetails {
    /// URI reference identifying the problem type.
    #[serde(rename = "type")]
    pub problem_type: String,
    /// Short summary.
    pub title: String,
    /// HTTP status code.
    pub status: u16,
    /// Human-readable explanation.
    pub detail: String,
    /// URI of the request that caused the error.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
    /// Detailed validation errors.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub errors: Vec<ValidationIssue>,
}

impl ProblemDetails {
    /// Create a validation error response wrapping a list of [`ValidationIssue`]s.
    #[must_use]
    pub fn validation_error(status: u16, issues: Vec<ValidationIssue>) -> Self {
        Self {
            problem_type: "about:blank".to_owned(),
            title: title_for_status(status).to_owned(),
            status,
            detail: "Request validation failed".to_owned(),
            instance: None,
            errors: issues,
        }
    }

    /// Create a 404 Not Found response.
    #[must_use]
    pub fn not_found(detail: &str) -> Self {
        Self {
            problem_type: "about:blank".to_owned(),
            title: "Not Found".to_owned(),
            status: 404,
            detail: detail.to_owned(),
            instance: None,
            errors: Vec::new(),
        }
    }

    /// Create a 415 Unsupported Media Type response.
    #[must_use]
    pub fn unsupported_media_type(detail: &str) -> Self {
        Self {
            problem_type: "about:blank".to_owned(),
            title: "Unsupported Media Type".to_owned(),
            status: 415,
            detail: detail.to_owned(),
            instance: None,
            errors: Vec::new(),
        }
    }

    /// Create a 413 Payload Too Large response.
    #[must_use]
    pub fn payload_too_large(detail: &str) -> Self {
        Self {
            problem_type: "about:blank".to_owned(),
            title: "Payload Too Large".to_owned(),
            status: 413,
            detail: detail.to_owned(),
            instance: None,
            errors: Vec::new(),
        }
    }
}

/// Map common HTTP status codes to their standard reason phrase.
const fn title_for_status(status: u16) -> &'static str {
    match status {
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        406 => "Not Acceptable",
        409 => "Conflict",
        413 => "Payload Too Large",
        415 => "Unsupported Media Type",
        422 => "Unprocessable Entity",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Unknown Error",
    }
}

/// Core error type.
#[derive(Debug, Error)]
pub enum SpecMockCoreError {
    /// Invalid schema document.
    #[error("schema compilation failed: {0}")]
    Schema(String),
    /// Data generation failed.
    #[error("data generation failed: {0}")]
    Faker(String),
    /// `$ref` resolution failed.
    #[error("$ref resolution failed: {0}")]
    Ref(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation_error_serialization_roundtrip() {
        let issues = vec![ValidationIssue {
            instance_pointer: "/name".to_owned(),
            schema_pointer: "/properties/name/minLength".to_owned(),
            keyword: "minLength".to_owned(),
            message: "must be at least 1 character".to_owned(),
        }];

        let problem = ProblemDetails::validation_error(400, issues);
        let json = serde_json::to_string(&problem).expect("serialize");
        let roundtrip: ProblemDetails = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(roundtrip.status, 400);
        assert_eq!(roundtrip.title, "Bad Request");
        assert_eq!(roundtrip.problem_type, "about:blank");
        assert_eq!(roundtrip.detail, "Request validation failed");
        assert!(roundtrip.instance.is_none());
        assert_eq!(roundtrip.errors.len(), 1);
        assert_eq!(roundtrip.errors[0].instance_pointer, "/name");
    }

    #[test]
    fn validation_error_json_shape() {
        let problem = ProblemDetails::validation_error(422, vec![]);
        let value = serde_json::to_value(&problem).expect("to_value");

        // `type` field must be present (serde rename)
        assert_eq!(value["type"], "about:blank");
        assert_eq!(value["status"], 422);
        assert_eq!(value["title"], "Unprocessable Entity");
        // empty errors array must be omitted
        assert!(value.get("errors").is_none());
    }

    #[test]
    fn not_found_has_correct_fields() {
        let problem = ProblemDetails::not_found("no such path: /pets/99");
        assert_eq!(problem.status, 404);
        assert_eq!(problem.title, "Not Found");
        assert_eq!(problem.detail, "no such path: /pets/99");
        assert!(problem.errors.is_empty());
    }

    #[test]
    fn unsupported_media_type_has_correct_fields() {
        let problem = ProblemDetails::unsupported_media_type("expected application/json");
        assert_eq!(problem.status, 415);
        assert_eq!(problem.title, "Unsupported Media Type");
    }

    #[test]
    fn payload_too_large_has_correct_fields() {
        let problem = ProblemDetails::payload_too_large("body exceeds 1 MB");
        assert_eq!(problem.status, 413);
        assert_eq!(problem.title, "Payload Too Large");
    }

    #[test]
    fn deserialize_without_optional_fields() {
        let json = r#"{"type":"about:blank","title":"Not Found","status":404,"detail":"gone"}"#;
        let problem: ProblemDetails = serde_json::from_str(json).expect("deserialize");
        assert_eq!(problem.status, 404);
        assert!(problem.instance.is_none());
        assert!(problem.errors.is_empty());
    }

    #[test]
    fn content_type_constant() {
        assert_eq!(PROBLEM_JSON_CONTENT_TYPE, "application/problem+json");
    }
}
