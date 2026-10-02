//! Media-type classification and response body encoding.
//!
//! OpenAPI responses may declare several media types per status code.  This
//! module classifies those media types and renders a JSON mock value into the
//! wire format the client negotiated.

use serde_json::Value;

/// Strip media-type parameters and normalize case.
///
/// `application/JSON; charset=utf-8` becomes `application/json`.
#[must_use]
pub fn normalize_media_type(raw: &str) -> String {
    let base = raw.split(';').next().unwrap_or(raw);
    base.trim().to_ascii_lowercase()
}

/// `true` for `application/json`, `application/problem+json`, and `*+json`.
#[must_use]
pub fn is_json_media_type(media_type: &str) -> bool {
    let normalized = normalize_media_type(media_type);
    normalized == "application/json" || normalized.ends_with("+json")
}

/// `true` for `application/xml`, `text/xml`, `application/xhtml+xml`, `*+xml`.
#[must_use]
pub fn is_xml_media_type(media_type: &str) -> bool {
    let normalized = normalize_media_type(media_type);
    normalized == "text/xml" ||
        normalized.ends_with("+xml") ||
        matches!(normalized.as_str(), "application/xml" | "application/xhtml+xml")
}

/// `true` for media types whose payload is textual rather than structured.
#[must_use]
pub fn is_text_media_type(media_type: &str) -> bool {
    if is_json_media_type(media_type) || is_xml_media_type(media_type) {
        return false;
    }
    let normalized = normalize_media_type(media_type);
    if normalized.starts_with("text/") {
        return true;
    }
    matches!(
        normalized.as_str(),
        "application/javascript" |
            "application/x-javascript" |
            "application/ecmascript" |
            "application/x-www-form-urlencoded" |
            "application/yaml" |
            "application/x-yaml"
    )
}

/// `true` for opaque binary payloads such as images, archives, and PDFs.
#[must_use]
pub fn is_binary_media_type(media_type: &str) -> bool {
    let normalized = normalize_media_type(media_type);
    normalized.starts_with("image/") ||
        normalized.starts_with("audio/") ||
        normalized.starts_with("video/") ||
        matches!(
            normalized.as_str(),
            "application/octet-stream" | "application/pdf" | "application/zip" | "application/gzip"
        )
}

/// `true` when `actual` is acceptable for one of the `declared` media types.
///
/// Matching honours exact names, `type/*` and `*/*` wildcards, and treats
/// `application/json` as satisfying a declared `application/<sub>+json`.
#[must_use]
pub fn media_type_is_accepted(declared: &[String], actual: &str) -> bool {
    let actual = normalize_media_type(actual);
    if actual.is_empty() {
        return false;
    }
    declared
        .iter()
        .any(|candidate| single_media_type_matches(&normalize_media_type(candidate), &actual))
}

fn single_media_type_matches(declared: &str, actual: &str) -> bool {
    if declared == actual || declared == "*/*" {
        return true;
    }
    if let Some(prefix) = declared.strip_suffix("/*") {
        return actual.strip_prefix(prefix).is_some_and(|suffix| suffix.contains('/'));
    }
    // A generic JSON request satisfies a structured-syntax `+json` declaration.
    actual == "application/json" &&
        declared.starts_with("application/") &&
        declared.ends_with("+json")
}

/// Render a JSON mock value into the wire bytes for `media_type`.
///
/// `schema` is the JSON schema the value was generated from.  It is only used
/// to derive a root element name for XML payloads.
///
/// Encoding rules:
/// - JSON media types: compact JSON.
/// - XML media types: a single root element derived from the schema.
/// - Binary media types: the raw bytes of a string value, otherwise JSON.
/// - Textual and unknown media types: the raw string value, otherwise JSON.
#[must_use]
pub fn encode_body(media_type: &str, value: &Value, schema: Option<&Value>) -> Vec<u8> {
    if is_json_media_type(media_type) {
        return serde_json::to_vec(value).unwrap_or_default();
    }
    if is_xml_media_type(media_type) {
        let root = xml_root_name(schema);
        return json_to_xml(&root, value).into_bytes();
    }
    match value {
        Value::String(text) => text.clone().into_bytes(),
        Value::Null => Vec::new(),
        other => serde_json::to_vec(other).unwrap_or_default(),
    }
}

/// Derive the XML root element name from a schema.
///
/// Prefers `xml.name`, then `title`, and falls back to `root`.
#[must_use]
pub fn xml_root_name(schema: Option<&Value>) -> String {
    let Some(schema) = schema else {
        return "root".to_owned();
    };
    schema
        .get("xml")
        .and_then(|xml| xml.get("name"))
        .and_then(Value::as_str)
        .or_else(|| schema.get("title").and_then(Value::as_str))
        .map_or_else(|| "root".to_owned(), ToOwned::to_owned)
}

/// Render a JSON value as an XML document with a single root element.
///
/// Objects become nested elements, arrays become a wrapper element containing
/// repeated `<item>` children, and scalars become element text.  Text is
/// XML-escaped.
#[must_use]
pub fn json_to_xml(root_name: &str, value: &Value) -> String {
    let mut out = String::new();
    write_xml_element(&mut out, &sanitize_xml_name(root_name), value);
    out
}

fn write_xml_element(out: &mut String, name: &str, value: &Value) {
    match value {
        Value::Object(map) => {
            open_xml_element(out, name);
            for (key, nested) in map {
                write_xml_element(out, &sanitize_xml_name(key), nested);
            }
            close_xml_element(out, name);
        }
        Value::Array(items) => {
            open_xml_element(out, name);
            for item in items {
                write_xml_element(out, "item", item);
            }
            close_xml_element(out, name);
        }
        scalar => {
            open_xml_element(out, name);
            out.push_str(&escape_xml_text(&scalar_text(scalar)));
            close_xml_element(out, name);
        }
    }
}

fn open_xml_element(out: &mut String, name: &str) {
    out.push('<');
    out.push_str(name);
    out.push('>');
}

fn close_xml_element(out: &mut String, name: &str) {
    out.push_str("</");
    out.push_str(name);
    out.push('>');
}

fn scalar_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn escape_xml_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            other => out.push(other),
        }
    }
    out
}

/// Coerce an arbitrary key into a syntactically valid XML element name.
fn sanitize_xml_name(raw: &str) -> String {
    let mut name = String::with_capacity(raw.len());
    for character in raw.chars() {
        if name.is_empty() {
            if character.is_ascii_alphabetic() || character == '_' {
                name.push(character);
            }
        } else if character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.') {
            name.push(character);
        }
    }
    if name.is_empty() { "_".to_owned() } else { name }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    // ── normalization / classification ──────────────────────────────────

    #[test]
    fn normalize_strips_parameters_and_lowercases() {
        assert_eq!(normalize_media_type("Application/JSON; charset=UTF-8"), "application/json");
        assert_eq!(normalize_media_type("text/plain"), "text/plain");
    }

    #[test]
    fn json_media_types_are_recognized() {
        assert!(is_json_media_type("application/json"));
        assert!(is_json_media_type("application/json; charset=utf-8"));
        assert!(is_json_media_type("application/problem+json"));
        assert!(!is_json_media_type("text/plain"));
        assert!(!is_json_media_type("application/xml"));
    }

    #[test]
    fn xml_media_types_are_recognized() {
        assert!(is_xml_media_type("application/xml"));
        assert!(is_xml_media_type("text/xml"));
        assert!(is_xml_media_type("application/soap+xml"));
        assert!(!is_xml_media_type("application/json"));
    }

    #[test]
    fn text_and_binary_media_types_are_disjoint() {
        assert!(is_text_media_type("text/plain"));
        assert!(is_text_media_type("text/event-stream"));
        assert!(!is_text_media_type("application/json"));
        assert!(is_binary_media_type("application/octet-stream"));
        assert!(is_binary_media_type("image/png"));
        assert!(!is_binary_media_type("text/plain"));
    }

    // ── accept matching ─────────────────────────────────────────────────

    #[test]
    fn accepts_exact_declared_type() {
        let declared = vec!["application/json".to_owned()];
        assert!(media_type_is_accepted(&declared, "application/json"));
        assert!(media_type_is_accepted(&declared, "application/json; charset=utf-8"));
        assert!(!media_type_is_accepted(&declared, "text/plain"));
    }

    #[test]
    fn accepts_wildcard_declarations() {
        assert!(media_type_is_accepted(&["*/*".to_owned()], "text/plain"));
        assert!(media_type_is_accepted(&["text/*".to_owned()], "text/csv"));
        assert!(!media_type_is_accepted(&["text/*".to_owned()], "application/json"));
    }

    #[test]
    fn json_satisfies_structured_json_declaration() {
        let declared = vec!["application/vnd.api+json".to_owned()];
        assert!(media_type_is_accepted(&declared, "application/json"));
        assert!(!media_type_is_accepted(&declared, "application/xml"));
    }

    #[test]
    fn missing_content_type_is_rejected_when_types_declared() {
        assert!(!media_type_is_accepted(&["application/json".to_owned()], ""));
    }

    // ── encoding ───────────────────────────────────────────────────────

    #[test]
    fn encodes_json_body() {
        let encoded = encode_body("application/json", &json!({"id": 1}), None);
        assert_eq!(encoded, br#"{"id":1}"#.to_vec());
    }

    #[test]
    fn encodes_text_body_verbatim() {
        let encoded = encode_body("text/plain", &json!("Monthly Report"), None);
        assert_eq!(encoded, b"Monthly Report".to_vec());
    }

    #[test]
    fn encodes_structured_value_as_json_for_text_media_type() {
        let encoded = encode_body("text/plain", &json!({"a": 1}), None);
        assert_eq!(encoded, br#"{"a":1}"#.to_vec());
    }

    #[test]
    fn encodes_binary_body_verbatim() {
        let encoded = encode_body("application/octet-stream", &json!("raw-bytes"), None);
        assert_eq!(encoded, b"raw-bytes".to_vec());
    }

    #[test]
    fn encodes_xml_object_with_schema_root_name() {
        let schema = json!({"type": "object", "xml": {"name": "Pet"}});
        let encoded = encode_body("application/xml", &json!({"name": "Rex"}), Some(&schema));
        assert_eq!(String::from_utf8(encoded).unwrap_or_default(), "<Pet><name>Rex</name></Pet>");
    }

    #[test]
    fn encodes_xml_array_with_item_elements() {
        let encoded = encode_body("application/xml", &json!({"tags": ["a", "b"]}), None);
        assert_eq!(
            String::from_utf8(encoded).unwrap_or_default(),
            "<root><tags><item>a</item><item>b</item></tags></root>"
        );
    }

    #[test]
    fn xml_text_is_escaped() {
        let encoded = json_to_xml("root", &json!({"note": "a & b < c"}));
        assert_eq!(encoded, "<root><note>a &amp; b &lt; c</note></root>");
    }

    #[test]
    fn xml_names_are_sanitized() {
        // Object keys are ordered by `serde_json`, so `""` sorts before `"bad key!"`.
        let encoded = json_to_xml("root", &json!({"bad key!": "v", "": "w"}));
        assert_eq!(encoded, "<root><_>w</_><badkey>v</badkey></root>");
    }

    #[test]
    fn xml_array_root_wraps_items() {
        let encoded = json_to_xml("root", &json!([1, 2]));
        assert_eq!(encoded, "<root><item>1</item><item>2</item></root>");
    }

    #[test]
    fn xml_scalar_root_uses_text_content() {
        assert_eq!(json_to_xml("name", &json!("Rex")), "<name>Rex</name>");
    }

    #[test]
    fn null_encodes_to_empty_body() {
        assert!(encode_body("text/plain", &Value::Null, None).is_empty());
    }

    #[test]
    fn root_name_falls_back_to_root() {
        assert_eq!(xml_root_name(None), "root");
        assert_eq!(xml_root_name(Some(&json!({"type": "object"}))), "root");
        assert_eq!(xml_root_name(Some(&json!({"title": "Report"}))), "Report");
    }
}
