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
    // `text` and `binary` share one path: a string value is emitted verbatim,
    // which is how a spec supplies opaque content. `null` is an empty body and
    // any other value falls back to its JSON form.
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

    // ── classification edge cases ──────────────────────────────────────

    #[test]
    fn classification_normalizes_case_and_parameters_first() {
        for variant in [
            "APPLICATION/JSON; CHARSET=UTF-8",
            " application/json ",
            "Application/Json;charset=utf-8",
        ] {
            assert!(is_json_media_type(variant), "{variant} should be JSON");
        }
        assert!(is_xml_media_type("APPLICATION/XML"), "case must not matter");
        assert!(is_text_media_type("TEXT/PLAIN; charset=utf-8"), "case must not matter");
        assert!(is_binary_media_type("APPLICATION/PDF; qs=0.9"), "parameters must be ignored");
    }

    #[test]
    fn structured_suffix_types_are_classified_by_suffix() {
        assert!(is_json_media_type("application/vnd.api+json"));
        assert!(is_json_media_type("application/hal+json"));
        assert!(is_xml_media_type("application/soap+xml"));
        assert!(is_xml_media_type("application/xhtml+xml"));
    }

    #[test]
    fn structured_types_are_never_treated_as_text() {
        assert!(!is_text_media_type("application/problem+json"));
        assert!(!is_text_media_type("application/atom+xml"));
        assert!(!is_text_media_type("application/json"));
    }

    #[test]
    fn additional_text_media_types_are_recognized() {
        for media_type in [
            "application/javascript",
            "application/x-javascript",
            "application/ecmascript",
            "application/x-www-form-urlencoded",
            "application/yaml",
            "application/x-yaml",
            "text/csv",
            "text/html",
            "text/event-stream",
        ] {
            assert!(is_text_media_type(media_type), "{media_type} should be text");
            assert!(!is_binary_media_type(media_type), "{media_type} should not be binary");
        }
    }

    #[test]
    fn additional_binary_media_types_are_recognized() {
        for media_type in [
            "image/png",
            "image/svg+xml",
            "audio/mpeg",
            "video/mp4",
            "application/octet-stream",
            "application/pdf",
            "application/zip",
            "application/gzip",
        ] {
            assert!(is_binary_media_type(media_type), "{media_type} should be binary");
        }
    }

    #[test]
    fn unknown_media_types_are_neither_text_nor_binary() {
        for media_type in ["application/vnd.custom", "", "nonsense", "*/*"] {
            assert!(!is_text_media_type(media_type), "{media_type} should not be text");
            assert!(!is_binary_media_type(media_type), "{media_type} should not be binary");
            assert!(!is_json_media_type(media_type), "{media_type} should not be json");
            assert!(!is_xml_media_type(media_type), "{media_type} should not be xml");
        }
    }

    #[test]
    fn normalization_of_an_empty_string_is_empty() {
        assert_eq!(normalize_media_type(""), "");
        assert_eq!(normalize_media_type(";"), "");
    }

    // ── accept matching ─────────────────────────────────────────────────

    #[test]
    fn accept_matching_is_case_and_parameter_insensitive() {
        assert!(media_type_is_accepted(&["APPLICATION/JSON".to_owned()], "application/json"));
        assert!(media_type_is_accepted(
            &["application/json".to_owned()],
            "APPLICATION/JSON; charset=utf-8"
        ));
    }

    #[test]
    fn accept_matching_rejects_an_empty_declared_list() {
        assert!(!media_type_is_accepted(&[], "application/json"));
    }

    #[test]
    fn accept_matching_requires_a_slash_after_the_subtype_wildcard() {
        // `text/*` matches `text/csv` but must not match a bare `text`.
        assert!(media_type_is_accepted(&["text/*".to_owned()], "text/csv"));
        assert!(!media_type_is_accepted(&["text/*".to_owned()], "text"));
        assert!(!media_type_is_accepted(&["text/*".to_owned()], "csv"));
    }

    #[test]
    fn accept_matching_only_grants_the_json_shortcut_for_application_types() {
        assert!(media_type_is_accepted(
            &["application/vnd.api+json".to_owned()],
            "application/json"
        ));
        assert!(
            !media_type_is_accepted(&["text/vnd.api+json".to_owned()], "application/json"),
            "the structured-JSON shortcut only applies under `application/`"
        );
    }

    #[test]
    fn accept_matching_finds_any_declared_candidate() {
        let declared = vec!["application/xml".to_owned(), "text/plain".to_owned()];
        assert!(media_type_is_accepted(&declared, "text/plain"));
        assert!(media_type_is_accepted(&declared, "application/xml"));
        assert!(!media_type_is_accepted(&declared, "application/json"));
    }

    // ── encoding ───────────────────────────────────────────────────────

    #[test]
    fn structured_json_suffixes_encode_as_json() {
        for media_type in ["application/problem+json", "application/vnd.api+json"] {
            let encoded = encode_body(media_type, &json!({"a": 1}), None);
            assert_eq!(encoded, br#"{"a":1}"#.to_vec(), "{media_type} must encode as JSON");
        }
    }

    #[test]
    fn encoded_json_is_compact_and_ordered() {
        let encoded = encode_body("application/json", &json!({"b": 1, "a": 2}), None);
        assert_eq!(encoded, br#"{"a":2,"b":1}"#.to_vec(), "keys are emitted in sorted order");
    }

    #[test]
    fn binary_null_encodes_to_an_empty_body() {
        assert!(encode_body("application/octet-stream", &Value::Null, None).is_empty());
    }

    #[test]
    fn binary_structured_value_falls_back_to_json() {
        let encoded = encode_body("application/octet-stream", &json!({"a": 1}), None);
        assert_eq!(encoded, br#"{"a":1}"#.to_vec());
    }

    #[test]
    fn unknown_media_type_encodes_a_string_verbatim() {
        let encoded = encode_body("application/vnd.custom", &json!("raw"), None);
        assert_eq!(encoded, b"raw".to_vec());
    }

    #[test]
    fn array_root_name_is_preserved() {
        assert_eq!(json_to_xml("items", &json!([])), "<items></items>");
        assert_eq!(json_to_xml("items", &json!([null])), "<items><item></item></items>");
    }

    #[test]
    fn xml_root_name_prefers_xml_name_over_title() {
        let schema = json!({"xml": {"name": "FromXml"}, "title": "FromTitle"});
        assert_eq!(xml_root_name(Some(&schema)), "FromXml");
    }

    #[test]
    fn xml_root_name_ignores_non_string_values() {
        let schema = json!({"xml": {"name": 42}, "title": 42});
        assert_eq!(xml_root_name(Some(&schema)), "root");
    }

    #[test]
    fn xml_root_name_falls_back_past_a_malformed_xml_node() {
        assert_eq!(xml_root_name(Some(&json!({"xml": "not an object", "title": "T"}))), "T");
        assert_eq!(xml_root_name(Some(&json!({"xml": {}}))), "root");
    }

    #[test]
    fn nested_xml_structures_are_rendered_recursively() {
        let value = json!({
            "outer": {
                "middle": {"inner": "deep", "count": 3, "flag": true, "nothing": null},
                "list": [1, 2]
            }
        });
        assert_eq!(
            json_to_xml("root", &value),
            "<root><outer><list><item>1</item><item>2</item></list>\
             <middle><count>3</count><flag>true</flag><inner>deep</inner><nothing></nothing></middle>\
             </outer></root>"
        );
    }

    #[test]
    fn empty_object_and_array_render_without_children() {
        assert_eq!(json_to_xml("root", &json!({})), "<root></root>");
        assert_eq!(json_to_xml("root", &json!([])), "<root></root>");
    }

    #[test]
    fn xml_escapes_every_reserved_character() {
        assert_eq!(
            json_to_xml("root", &json!({"v": "&<>\""})),
            r#"<root><v>&amp;&lt;&gt;&quot;</v></root>"#
        );
        assert_eq!(json_to_xml("root", &json!({"v": "'"})), "<root><v>&apos;</v></root>");
    }

    #[test]
    fn xml_names_drop_invalid_characters_and_require_a_valid_start() {
        assert_eq!(json_to_xml("root", &json!({"1abc": "v"})), "<root><abc>v</abc></root>");
        assert_eq!(json_to_xml("root", &json!({"_ok": "v"})), "<root><_ok>v</_ok></root>");
        assert_eq!(
            json_to_xml("root", &json!({"a-b.c_d": "v"})),
            "<root><a-b.c_d>v</a-b.c_d></root>",
            "dashes, dots and underscores are legal inside a name"
        );
        assert_eq!(
            json_to_xml("1root", &json!("v")),
            "<root>v</root>",
            "the root name is sanitized too"
        );
        assert_eq!(
            json_to_xml("", &json!("v")),
            "<_>v</_>",
            "an unnameable root falls back to `_`"
        );
        assert_eq!(json_to_xml("!!!", &json!("v")), "<_>v</_>");
    }

    #[test]
    fn unicode_keys_are_dropped_from_xml_names() {
        // `路径` has no ASCII characters, so the sanitized name becomes `_`.
        assert_eq!(json_to_xml("root", &json!({"路径": "v"})), "<root><_>v</_></root>");
    }

    #[test]
    fn xml_scalar_numbers_render_unquoted() {
        assert_eq!(json_to_xml("root", &json!({"n": -1.5})), "<root><n>-1.5</n></root>");
        assert_eq!(json_to_xml("root", &json!({"n": null})), "<root><n></n></root>");
    }

    #[test]
    fn xml_encoding_through_encode_body_uses_the_schema_root() {
        let schema = json!({"xml": {"name": "Pet"}});
        let encoded = encode_body("text/xml", &json!([{"a": 1}]), Some(&schema));
        assert_eq!(
            String::from_utf8(encoded).unwrap_or_default(),
            "<Pet><item><a>1</a></item></Pet>"
        );
    }

    // ── property tests ─────────────────────────────────────────────────

    use proptest::prelude::*;

    /// Recursive strategy for arbitrary JSON values.
    ///
    /// `serde_json::Value` does not implement `proptest::Arbitrary`, so the
    /// generator is assembled from bounded leaf and container shapes.
    fn any_json() -> impl Strategy<Value = serde_json::Value> {
        let leaf = prop_oneof![
            any::<bool>().prop_map(serde_json::Value::from),
            any::<i64>().prop_map(serde_json::Value::from),
            "[ -~]{0,8}".prop_map(serde_json::Value::from),
        ];
        leaf.prop_recursive(3, 12, 3, |inner| {
            prop_oneof![
                prop::collection::vec(inner.clone(), 0..4).prop_map(serde_json::Value::Array),
                prop::collection::hash_map("[a-z]{1,6}", inner, 0..4).prop_map(|members| {
                    let map: serde_json::Map<String, serde_json::Value> =
                        members.into_iter().collect();
                    serde_json::Value::Object(map)
                }),
            ]
        })
    }

    // `rustfmt` cannot parse the `proptest!` DSL, so this block is left verbatim.

    #[rustfmt::skip]

    proptest! {
        /// The JSON/XML/text/binary classification is total and the JSON and
        /// XML families never overlap.
        #[test]
        fn classification_never_panics_and_families_are_disjoint(
            media_type in "[a-zA-Z0-9*+./;-]{0,30}"
        ) {
            let json = is_json_media_type(&media_type);
            let xml = is_xml_media_type(&media_type);
            let text = is_text_media_type(&media_type);
            prop_assert!(!(json && xml), "{} cannot be both JSON and XML", media_type);
            prop_assert!(!(json && text), "{} cannot be both JSON and text", media_type);
            prop_assert!(!(xml && text), "{} cannot be both XML and text", media_type);
        }

        /// Whatever the declared list and incoming type, accept matching is
        /// total and never panics.
        #[test]
        fn accept_matching_never_panics(
            declared in prop::collection::vec("[a-z]{1,6}/[a-z0-9+.*-]{1,10}", 0..5),
            actual in "[a-z]{1,6}/[a-z0-9+.-]{0,10}",
        ) {
            let _ = media_type_is_accepted(&declared, &actual);
        }

        /// An exactly-matching declared type always accepts the actual type.
        #[test]
        fn exact_declaration_always_accepts(
            media_type in "[a-z]{1,6}/[a-z0-9+.-]{1,10}",
        ) {
            let declared = vec![media_type.clone()];
            prop_assert!(media_type_is_accepted(&declared, &media_type));
            prop_assert!(
                media_type_is_accepted(&declared, &format!("{media_type}; charset=utf-8")),
                "{} with parameters must still match its exact declaration",
                media_type
            );
        }

        /// The wildcard declaration accepts every concrete media type.
        #[test]
        fn wildcard_always_accepts(actual in "[a-z]{1,6}/[a-z0-9+.-]{1,10}") {
            prop_assert!(media_type_is_accepted(&["*/*".to_owned()], &actual));
        }

        /// XML output must always be single-rooted with matching open/close
        /// tags, whatever the input value shape.
        #[test]
        fn xml_output_is_always_balanced(value in any_json()) {
            let rendered = json_to_xml("root", &value);
            let opens = rendered.matches('<').count();
            let closes = rendered.matches('>').count();
            prop_assert_eq!(opens, closes, "unbalanced angle brackets in {:?}", rendered);
            prop_assert!(
                rendered.starts_with('<'),
                "output is not rooted in an element: {:?}",
                rendered
            );
            prop_assert!(
                rendered.contains("</"),
                "no closing tag at all in {rendered:?}"
            );
        }

        /// Raw XML metacharacters in string values never survive unescaped.
        #[test]
        fn xml_escaping_is_complete(text in "[&<>\"']{0,8}") {
            const ENTITIES: [&str; 5] = ["&amp;", "&lt;", "&gt;", "&quot;", "&apos;"];

            let rendered = json_to_xml("root", &json!({"v": text.clone()}));
            let inner = rendered
                .strip_prefix("<root><v>")
                .and_then(|rest| rest.strip_suffix("</v></root>"))
                .unwrap_or("");

            prop_assert!(!inner.contains('<'), "raw '<' survived escaping: {:?}", inner);
            prop_assert!(!inner.contains('>'), "raw '>' survived escaping: {:?}", inner);

            for (index, _) in inner.match_indices('&') {
                let rest = &inner[index..];
                prop_assert!(
                    ENTITIES.iter().any(|entity| rest.starts_with(entity)),
                    "unescaped '&' in {inner:?}"
                );
            }
        }

        /// Encoding is total: any media type plus any value produces bytes.
        #[test]
        fn encoding_never_panics(
            value in any_json(),
            media_type in "[a-z]{1,6}/[a-z0-9+.-]{1,10}",
            schema in prop::option::of(any_json()),
        ) {
            let _ = encode_body(&media_type, &value, schema.as_ref());
        }

        /// JSON media types always produce valid JSON bytes.
        #[test]
        fn json_media_types_produce_valid_json(
            value in any_json(),
            media_type in prop::sample::select(vec![
                "application/json".to_owned(),
                "application/problem+json".to_owned(),
                "application/vnd.api+json".to_owned(),
            ]),
        ) {
            let encoded = encode_body(&media_type, &value, None);
            let text = std::str::from_utf8(&encoded).expect("JSON output must be UTF-8");
            let decoded: serde_json::Value =
                serde_json::from_str(text).expect("JSON output must re-parse");
            prop_assert_eq!(&decoded, &value);
        }
    }
}
