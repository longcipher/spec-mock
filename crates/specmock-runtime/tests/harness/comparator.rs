use serde_json::Value;

use super::request::CapturedResponse;

// ─── Request category ────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RequestCategory {
    ValidFuzz,
    InvalidPathParam,
    MissingRequiredParam,
    InvalidBody,
    WrongContentType,
    UnknownPath,
    PreferCode(u16),
    PreferExample(String),
    PreferDynamic,
    AcceptNegotiation,
}

// ─── Diff types ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DiffKind {
    /// HTTP status codes differ.
    StatusMismatch,
    /// Base content-type strings differ.
    ContentTypeMismatch,
    /// Key is present in the Prism response but absent in the spec-mock response.
    MissingKey,
    /// Key is present in the spec-mock response but absent in the Prism response.
    ExtraKey,
    /// Same key but different JSON type.
    TypeMismatch,
    /// Arrays with the same path have different lengths.
    ArrayLengthMismatch,
    /// An RFC 7807 required field (`type`, `title`, or `status`) differs.
    Rfc7807FieldMismatch,
}

#[derive(Debug, Clone)]
pub(crate) struct Diff {
    /// JSON pointer-like path to the differing value, e.g. `$.status` or `$.body.id`.
    pub(crate) path: String,
    pub(crate) kind: DiffKind,
    /// spec-mock side.
    pub(crate) actual: String,
    /// Prism side.
    pub(crate) expected: String,
}

// ─── ComparisonResult ────────────────────────────────────────────────────────

#[derive(Debug)]
pub(crate) struct ComparisonResult {
    pub(crate) diffs: Vec<Diff>,
    pub(crate) description: String,
    #[expect(dead_code, reason = "used by future prism comparison tests")]
    pub(crate) category: RequestCategory,
    pub(crate) specmock_status: u16,
    pub(crate) prism_status: u16,
}

impl ComparisonResult {
    /// Returns `true` only when there are no diffs.
    pub(crate) const fn is_match(&self) -> bool {
        self.diffs.is_empty()
    }

    /// Returns a human-readable report string.
    pub(crate) fn report(&self) -> String {
        let mut lines = if self.is_match() {
            vec![format!("PASS: {}", self.description)]
        } else {
            vec![
                format!("FAIL: {}", self.description),
                format!("  spec-mock status: {}", self.specmock_status),
                format!("  prism status: {}", self.prism_status),
                format!("  Diffs ({}):", self.diffs.len()),
            ]
        };
        for diff in &self.diffs {
            lines.push(format!(
                "    [{}] {:?}: specmock={}, prism={}",
                diff.path, diff.kind, diff.actual, diff.expected
            ));
        }
        lines.join("\n")
    }
}

// ─── Comparison helpers ──────────────────────────────────────────────────────

/// Compare status codes. Returns `Some(Diff)` on mismatch.
pub(crate) fn compare_status(specmock: u16, prism: u16) -> Option<Diff> {
    if specmock == prism {
        return None;
    }
    Some(Diff {
        path: "$.status".to_owned(),
        kind: DiffKind::StatusMismatch,
        actual: specmock.to_string(),
        expected: prism.to_string(),
    })
}

/// Strip the charset/parameter suffix from a Content-Type value.
///
/// `"application/json; charset=utf-8"` → `"application/json"`
fn base_content_type(ct: &str) -> &str {
    ct.split(';').next().unwrap_or(ct).trim()
}

/// Compare Content-Type headers, ignoring charset/parameter suffixes.
/// Returns `Some(Diff)` when the *base* types differ.
pub(crate) fn compare_content_type(specmock: Option<&str>, prism: Option<&str>) -> Option<Diff> {
    let sm_base = specmock.map(base_content_type);
    let prism_base = prism.map(base_content_type);
    if sm_base == prism_base {
        return None;
    }
    Some(Diff {
        path: "$.content_type".to_owned(),
        kind: DiffKind::ContentTypeMismatch,
        actual: sm_base.unwrap_or("(none)").to_owned(),
        expected: prism_base.unwrap_or("(none)").to_owned(),
    })
}

/// Return the human-readable JSON type name for a `Value`.
const fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Recursively compare JSON structures for *structural* equivalence.
///
/// Rules:
/// - Key presence: `MissingKey` / `ExtraKey`.
/// - JSON type divergence: `TypeMismatch`.
/// - Scalar values are **not** compared — two different integers are structurally equal.
/// - Array lengths are compared (`ArrayLengthMismatch` on mismatch); the first element type of both
///   arrays is compared when both are non-empty.
/// - Object fields are recursed.
/// - `path` is the JSON-path prefix for the current level (e.g. `"$"` for the root, `"$.pet"` for a
///   nested field).
pub(crate) fn compare_json_structure(specmock: &Value, prism: &Value, path: &str) -> Vec<Diff> {
    let mut diffs = Vec::new();

    match (specmock, prism) {
        // Both null — structurally equal.
        (Value::Null, Value::Null) => {}

        // Same scalar types — structurally equal (values not compared).
        (Value::Bool(_), Value::Bool(_)) |
        (Value::Number(_), Value::Number(_)) |
        (Value::String(_), Value::String(_)) => {}

        // Both objects — compare key sets and recurse.
        (Value::Object(sm_map), Value::Object(prism_map)) => {
            for key in prism_map.keys() {
                if !sm_map.contains_key(key) {
                    diffs.push(Diff {
                        path: format!("{path}.{key}"),
                        kind: DiffKind::MissingKey,
                        actual: "(absent)".to_owned(),
                        expected: prism_map[key].to_string(),
                    });
                }
            }
            for key in sm_map.keys() {
                if !prism_map.contains_key(key) {
                    diffs.push(Diff {
                        path: format!("{path}.{key}"),
                        kind: DiffKind::ExtraKey,
                        actual: sm_map[key].to_string(),
                        expected: "(absent)".to_owned(),
                    });
                }
            }
            for key in sm_map.keys() {
                if let Some(prism_val) = prism_map.get(key) {
                    let nested = format!("{path}.{key}");
                    diffs.extend(compare_json_structure(&sm_map[key], prism_val, &nested));
                }
            }
        }

        // Both arrays — compare length, then element type.
        (Value::Array(sm_arr), Value::Array(prism_arr)) => {
            if sm_arr.len() != prism_arr.len() {
                diffs.push(Diff {
                    path: format!("{path}[]"),
                    kind: DiffKind::ArrayLengthMismatch,
                    actual: sm_arr.len().to_string(),
                    expected: prism_arr.len().to_string(),
                });
            }
            // Recurse into the first element pair when both are non-empty.
            if let (Some(sm_first), Some(prism_first)) = (sm_arr.first(), prism_arr.first()) {
                let nested = format!("{path}[0]");
                diffs.extend(compare_json_structure(sm_first, prism_first, &nested));
            }
        }

        // Type divergence.
        _ => {
            diffs.push(Diff {
                path: path.to_owned(),
                kind: DiffKind::TypeMismatch,
                actual: type_name(specmock).to_owned(),
                expected: type_name(prism).to_owned(),
            });
        }
    }

    diffs
}

/// Returns `true` if `v` looks like an RFC 7807 Problem Details object.
fn is_rfc7807(v: &Value) -> bool {
    let Some(obj) = v.as_object() else {
        return false;
    };
    // "type" with string value OR "status" with number value.
    obj.get("type").is_some_and(Value::is_string) || obj.get("status").is_some_and(Value::is_number)
}

/// Special comparison for RFC 7807 Problem Details objects.
///
/// Checks `"type"`, `"title"`, and `"status"` for exact equality.
/// `"detail"` and `"errors"` are only checked for *presence*, not content.
pub(crate) fn compare_rfc7807(specmock: &Value, prism: &Value) -> Vec<Diff> {
    let mut diffs = Vec::new();

    for field in &["type", "title", "status"] {
        let sm_val = specmock.get(field);
        let prism_val = prism.get(field);
        match (sm_val, prism_val) {
            (Some(a), Some(b)) if a != b => {
                diffs.push(Diff {
                    path: format!("$.body.{field}"),
                    kind: DiffKind::Rfc7807FieldMismatch,
                    actual: a.to_string(),
                    expected: b.to_string(),
                });
            }
            (None, Some(b)) => {
                diffs.push(Diff {
                    path: format!("$.body.{field}"),
                    kind: DiffKind::Rfc7807FieldMismatch,
                    actual: "(absent)".to_owned(),
                    expected: b.to_string(),
                });
            }
            (Some(a), None) => {
                diffs.push(Diff {
                    path: format!("$.body.{field}"),
                    kind: DiffKind::Rfc7807FieldMismatch,
                    actual: a.to_string(),
                    expected: "(absent)".to_owned(),
                });
            }
            _ => {}
        }
    }

    // Presence-only checks for "detail" and "errors".
    for field in &["detail", "errors"] {
        let sm_has = specmock.get(field).is_some();
        let prism_has = prism.get(field).is_some();
        if sm_has != prism_has {
            diffs.push(Diff {
                path: format!("$.body.{field}"),
                kind: DiffKind::Rfc7807FieldMismatch,
                actual: if sm_has { "(present)" } else { "(absent)" }.to_owned(),
                expected: if prism_has { "(present)" } else { "(absent)" }.to_owned(),
            });
        }
    }

    diffs
}

// ─── Top-level comparator ────────────────────────────────────────────────────

/// Compare a spec-mock response against the reference Prism response.
///
/// Logic:
/// 1. Compare status codes.
/// 2. Compare content types (charset suffix is stripped).
/// 3. Parse both bodies as JSON; skip body comparison when either is missing or not valid JSON.
/// 4. If both are JSON and the response looks like RFC 7807, use [`compare_rfc7807`]; otherwise use
///    [`compare_json_structure`].
pub(crate) fn compare_responses(
    specmock: &CapturedResponse,
    prism: &CapturedResponse,
    description: &str,
    category: RequestCategory,
) -> ComparisonResult {
    let mut diffs = Vec::new();

    // 1. Status.
    if let Some(d) = compare_status(specmock.status, prism.status) {
        diffs.push(d);
    }

    // 2. Content-Type.
    if let Some(d) =
        compare_content_type(specmock.content_type.as_deref(), prism.content_type.as_deref())
    {
        diffs.push(d);
    }

    // 3 & 4. Body.
    let sm_json: Option<Value> =
        if specmock.body.is_empty() { None } else { serde_json::from_slice(&specmock.body).ok() };
    let prism_json: Option<Value> =
        if prism.body.is_empty() { None } else { serde_json::from_slice(&prism.body).ok() };

    if let (Some(sm_val), Some(prism_val)) = (sm_json.as_ref(), prism_json.as_ref()) {
        if is_rfc7807(sm_val) || is_rfc7807(prism_val) {
            diffs.extend(compare_rfc7807(sm_val, prism_val));
        } else {
            diffs.extend(compare_json_structure(sm_val, prism_val, "$"));
        }
    }

    ComparisonResult {
        diffs,
        description: description.to_owned(),
        category,
        specmock_status: specmock.status,
        prism_status: prism.status,
    }
}

// ─── Unit tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_response(status: u16, ct: Option<&str>, body: &[u8]) -> CapturedResponse {
        CapturedResponse {
            status,
            content_type: ct.map(ToOwned::to_owned),
            body: body.to_vec(),
            headers: Vec::new(),
        }
    }

    #[test]
    fn identical_responses_pass() {
        let body = br#"{"id":1,"name":"Fido"}"#;
        let sm = make_response(200, Some("application/json"), body);
        let pr = make_response(200, Some("application/json"), body);
        let result = compare_responses(&sm, &pr, "identical", RequestCategory::ValidFuzz);
        assert!(result.is_match(), "expected match but got diffs: {:?}", result.diffs);
    }

    #[test]
    fn status_mismatch_produces_diff() {
        let sm = make_response(200, None, b"");
        let pr = make_response(404, None, b"");
        let result = compare_responses(&sm, &pr, "status mismatch", RequestCategory::ValidFuzz);
        assert!(
            result.diffs.iter().any(|d| d.kind == DiffKind::StatusMismatch),
            "expected StatusMismatch diff"
        );
    }

    #[test]
    fn missing_key_produces_diff() {
        let sm_body = br#"{"id":1,"name":"a"}"#;
        let prism_body = br#"{"id":1,"name":"a","extra":"b"}"#;
        let sm = make_response(200, Some("application/json"), sm_body);
        let pr = make_response(200, Some("application/json"), prism_body);
        let result = compare_responses(&sm, &pr, "missing key", RequestCategory::ValidFuzz);
        assert!(
            result.diffs.iter().any(|d| d.kind == DiffKind::MissingKey),
            "expected MissingKey diff, got: {:?}",
            result.diffs
        );
    }

    #[test]
    fn type_mismatch_produces_diff() {
        let sm_body = br#"{"id":"string"}"#;
        let prism_body = br#"{"id":1}"#;
        let sm = make_response(200, Some("application/json"), sm_body);
        let pr = make_response(200, Some("application/json"), prism_body);
        let result = compare_responses(&sm, &pr, "type mismatch", RequestCategory::ValidFuzz);
        assert!(
            result.diffs.iter().any(|d| d.kind == DiffKind::TypeMismatch),
            "expected TypeMismatch diff, got: {:?}",
            result.diffs
        );
    }

    #[test]
    fn content_type_comparison_strips_charset() {
        let sm = make_response(200, Some("application/json; charset=utf-8"), b"");
        let pr = make_response(200, Some("application/json"), b"");
        let result = compare_responses(&sm, &pr, "charset strip", RequestCategory::ValidFuzz);
        assert!(
            !result.diffs.iter().any(|d| d.kind == DiffKind::ContentTypeMismatch),
            "expected no ContentTypeMismatch diff when base types match"
        );
    }

    #[test]
    fn array_length_mismatch_produces_diff() {
        let sm_body = br"[1,2,3]";
        let prism_body = br"[1,2]";
        let sm = make_response(200, Some("application/json"), sm_body);
        let pr = make_response(200, Some("application/json"), prism_body);
        let result = compare_responses(&sm, &pr, "array length", RequestCategory::ValidFuzz);
        assert!(
            result.diffs.iter().any(|d| d.kind == DiffKind::ArrayLengthMismatch),
            "expected ArrayLengthMismatch diff, got: {:?}",
            result.diffs
        );
    }

    #[test]
    fn rfc7807_title_mismatch_produces_diff() {
        let sm_body = br#"{"type":"about:blank","title":"Bad Request","status":400}"#;
        let prism_body = br#"{"type":"about:blank","title":"Validation Error","status":400}"#;
        let sm = make_response(400, Some("application/problem+json"), sm_body);
        let pr = make_response(400, Some("application/problem+json"), prism_body);
        let result = compare_responses(&sm, &pr, "rfc7807 title", RequestCategory::InvalidBody);
        assert!(
            result.diffs.iter().any(|d| d.kind == DiffKind::Rfc7807FieldMismatch),
            "expected Rfc7807FieldMismatch diff, got: {:?}",
            result.diffs
        );
    }
}
