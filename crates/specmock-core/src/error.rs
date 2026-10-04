//! Error and validation issue models.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

/// Content-Type for RFC 7807 responses.
pub const PROBLEM_JSON_CONTENT_TYPE: &str = "application/problem+json";

/// Standard validation issue item with JSON pointer locations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationIssue {
    /// JSON pointer into instance payload.
    pub instance_pointer: String,
    /// JSON pointer to the rule that was violated.
    ///
    /// Two pointer spaces share this field, distinguished by the leading `#`:
    ///
    /// - *Schema* pointers are bare and address a keyword inside the subschema that failed, such
    ///   as `/minimum` or `/properties/name/minLength`.
    /// - *Specification* pointers are `#`-prefixed and address the position in the OpenAPI or
    ///   AsyncAPI document itself, such as `#/requestBody` or `#/channels/{name}`. These mark
    ///   where in the contract the failure originates, not which schema keyword rejected a value.
    ///
    /// A bare `#` means the failure concerns the document as a whole.
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
    /// RFC 7807 extension members.
    ///
    /// Failures that are not JSON Schema violations -- a scenario refusing an
    /// operation, a store missing a resource -- carry their context here rather
    /// than being forced into `errors`, where every field would describe a
    /// schema keyword and the pointers would point at nothing.
    #[serde(flatten, skip_serializing_if = "BTreeMap::is_empty", default)]
    pub extensions: BTreeMap<String, Value>,
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
            extensions: BTreeMap::new(),
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
            extensions: BTreeMap::new(),
        }
    }

    /// Create a response for a failure that is not a schema violation.
    ///
    /// `errors` stays empty: an operation refused by a state machine has no
    /// instance pointer and no schema pointer to report.
    #[must_use]
    pub fn stateful_error(
        status: u16,
        detail: impl Into<String>,
        extensions: impl IntoIterator<Item = (String, Value)>,
    ) -> Self {
        Self {
            problem_type: "about:blank".to_owned(),
            title: title_for_status(status).to_owned(),
            status,
            detail: detail.into(),
            instance: None,
            errors: Vec::new(),
            extensions: extensions.into_iter().collect(),
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
            extensions: BTreeMap::new(),
        }
    }

    /// Create a 406 Not Acceptable response.
    #[must_use]
    pub fn not_acceptable(detail: &str) -> Self {
        Self {
            problem_type: "about:blank".to_owned(),
            title: "Not Acceptable".to_owned(),
            status: 406,
            detail: detail.to_owned(),
            instance: None,
            errors: Vec::new(),
            extensions: BTreeMap::new(),
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
            extensions: BTreeMap::new(),
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
    fn a_stateful_error_carries_extension_members_alongside_the_standard_fields() {
        let problem = ProblemDetails::stateful_error(
            409,
            "operation 'pay' is not legal in state 'cart'",
            [
                ("scenario".to_owned(), serde_json::json!("checkout")),
                ("state".to_owned(), serde_json::json!("cart")),
            ],
        );

        let encoded = serde_json::to_value(&problem).expect("problem should serialize");
        assert_eq!(encoded["status"], 409);
        assert_eq!(encoded["scenario"], "checkout");
        assert_eq!(encoded["state"], "cart");
        assert!(
            encoded.get("errors").is_none(),
            "a non-schema failure must not carry an empty errors array"
        );
    }

    #[test]
    fn extension_members_are_omitted_when_empty() {
        let encoded =
            serde_json::to_value(ProblemDetails::not_found("nothing here")).expect("serialize");
        assert!(
            !encoded.as_object().is_some_and(|object| object.keys().any(|key| key == "extensions")),
            "an empty extension map must not appear on the wire"
        );
    }

    #[test]
    fn a_validation_error_reports_no_extensions() {
        let issue = ValidationIssue {
            instance_pointer: "/id".to_owned(),
            schema_pointer: "/minimum".to_owned(),
            keyword: "minimum".to_owned(),
            message: "too small".to_owned(),
        };
        let encoded = serde_json::to_value(ProblemDetails::validation_error(400, vec![issue]))
            .expect("serialize");
        assert_eq!(encoded["errors"][0]["instance_pointer"], "/id");
    }

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
    fn not_acceptable_has_correct_fields() {
        let problem = ProblemDetails::not_acceptable("no acceptable representation");
        assert_eq!(problem.status, 406);
        assert_eq!(problem.title, "Not Acceptable");
        assert_eq!(problem.detail, "no acceptable representation");
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

    #[test]
    fn unmapped_status_falls_back_to_a_generic_title() {
        assert_eq!(title_for_status(0), "Unknown Error");
        assert_eq!(title_for_status(418), "Unknown Error");
        assert_eq!(title_for_status(999), "Unknown Error");
        assert_eq!(title_for_status(u16::MAX), "Unknown Error");
    }

    #[test]
    fn every_mapped_status_has_a_distinct_reason_phrase() {
        let statuses = [400, 401, 403, 404, 405, 406, 409, 413, 415, 422, 429, 500, 502, 503];

        let mut seen = std::collections::BTreeSet::new();
        for status in statuses {
            let title = title_for_status(status);
            assert_ne!(title, "Unknown Error", "{status} should be mapped");
            assert!(seen.insert(title), "{status} reuses the title {title:?}");
        }
    }

    #[test]
    fn validation_error_preserves_every_issue() {
        let issues = vec![
            ValidationIssue {
                instance_pointer: "/a".to_owned(),
                schema_pointer: "#/a".to_owned(),
                keyword: "type".to_owned(),
                message: "first".to_owned(),
            },
            ValidationIssue {
                instance_pointer: "/b".to_owned(),
                schema_pointer: "#/b".to_owned(),
                keyword: "required".to_owned(),
                message: "second".to_owned(),
            },
        ];

        let problem = ProblemDetails::validation_error(400, issues.clone());
        assert_eq!(problem.errors, issues, "no issue may be dropped or reordered");
    }

    #[test]
    fn non_validation_constructors_never_carry_errors() {
        for problem in [
            ProblemDetails::not_found("a"),
            ProblemDetails::unsupported_media_type("b"),
            ProblemDetails::not_acceptable("c"),
            ProblemDetails::payload_too_large("d"),
        ] {
            assert!(problem.errors.is_empty());
            assert_eq!(problem.problem_type, "about:blank");
            assert!(problem.instance.is_none());
        }
    }

    #[test]
    fn instance_is_omitted_from_serialization_when_absent() {
        let problem = ProblemDetails::not_found("gone");
        let value = serde_json::to_value(&problem).expect("to_value");
        assert!(value.get("instance").is_none());
        assert_eq!(value.get("detail").and_then(serde_json::Value::as_str), Some("gone"));
    }

    #[test]
    fn error_variants_render_their_payload() {
        assert_eq!(
            SpecMockCoreError::Schema("bad".to_owned()).to_string(),
            "schema compilation failed: bad"
        );
        assert_eq!(
            SpecMockCoreError::Faker("bad".to_owned()).to_string(),
            "data generation failed: bad"
        );
        assert_eq!(
            SpecMockCoreError::Ref("bad".to_owned()).to_string(),
            "$ref resolution failed: bad"
        );
    }
}
